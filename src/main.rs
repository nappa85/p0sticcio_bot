use std::{
    collections::HashMap,
    env, future,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use chrono::Utc;

use futures_util::StreamExt;

use ingress_intel_rs::{
    entities::IntelResult,
    plexts::Tab,
    portal_details::{IntelMod, IntelPortal, IntelResonator},
    Intel,
};

use lru_time_cache::LruCache;

use once_cell::sync::Lazy;

use rust_decimal::{
    prelude::{FromPrimitive, ToPrimitive},
    Decimal,
};

use sea_orm::{ConnectionTrait, Database, TransactionTrait};
use serde_json::json;

use stream_throttle::{ThrottlePool, ThrottleRate, ThrottledStream};

use tokio::{sync::mpsc, time};
use tokio_stream::wrappers::UnboundedReceiverStream;

use tracing::{debug, error, info, warn};

mod config;
mod database;
mod dedup_flatten;
mod entities;
mod symbols;

type Senders = HashMap<u64, mpsc::UnboundedSender<Bot>>;

static USERNAME: Lazy<Option<String>> = Lazy::new(|| env::var("USERNAME").ok());
static PASSWORD: Lazy<Option<String>> = Lazy::new(|| env::var("PASSWORD").ok());
static COOKIES: Lazy<Option<String>> = Lazy::new(|| env::var("COOKIES").ok());
static BOT_URL1: Lazy<String> = Lazy::new(|| {
    format!("https://api.telegram.org/bot{}/sendMessage", env::var("BOT_TOKEN1").expect("Missing env var BOT_TOKEN1"))
});
static BOT_URL2: Lazy<String> = Lazy::new(|| {
    format!("https://api.telegram.org/bot{}/sendMessage", env::var("BOT_TOKEN2").expect("Missing env var BOT_TOKEN2"))
});
static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .expect("Client build failed")
});
static SCANNING: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
enum Bot {
    Comm(String),
    Portal(String),
}

impl Bot {
    fn get_msg(&self) -> &str {
        match self {
            Bot::Comm(s) => s.as_str(),
            Bot::Portal(s) => s.as_str(),
        }
    }
    fn get_url(&self) -> &str {
        match self {
            Bot::Comm(_) => BOT_URL1.as_str(),
            Bot::Portal(_) => BOT_URL2.as_str(),
        }
    }
}

#[tokio::main]
async fn main() {
    println!("Started version {}", env!("CARGO_PKG_VERSION"));
    tracing_subscriber::fmt::init();

    let config = config::get().await.expect("Config read failed");

    let conn =
        Database::connect("mysql://mariadb:mariadb@mariadb/p0sticcio_bot").await.expect("Database connection failed");

    let (global_tx, global_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        // we can send globally only 30 telegram messages per second
        let rate = ThrottleRate::new(30, Duration::from_secs(1));
        let pool = ThrottlePool::new(rate);
        UnboundedReceiverStream::new(global_rx)
            .throttle(pool)
            .for_each_concurrent(None, |(user_id, msg): (u64, Bot)| async move {
                for i in 0..10 {
                    let res = CLIENT
                        .post(msg.get_url())
                        .header("Content-Type", "application/json")
                        .json(&json!({
                            "chat_id": user_id,
                            "text": msg.get_msg(),
                            "parse_mode": "HTML",
                            "disable_web_page_preview": true
                        }))
                        .send()
                        .await
                        .map_err(|e| {
                            error!("Telegram error on retry {}: {}\nuser_id: {}\nmessage: {:?}", i, e, user_id, msg)
                        });
                    if let Ok(res) = res {
                        if res.status().is_success() {
                            break;
                        }

                        error!(
                            "Telegram error on retry {}: {:?}\nuser_id: {}\nmessage: {:?}",
                            i,
                            res.text().await,
                            user_id,
                            msg
                        );
                    }
                    time::sleep(Duration::from_secs(1)).await;
                }
            })
            .await;
    });

    let senders = config
        .zones
        .iter()
        .flat_map(|z| {
            z.users.keys().map(|id| {
                let (tx, rx) = mpsc::unbounded_channel();
                let global_tx = global_tx.clone();
                let id = *id;
                tokio::spawn(async move {
                    // We can send a single message per telegram chat per second
                    let rate = ThrottleRate::new(1, Duration::from_secs(1));
                    let pool = ThrottlePool::new(rate);
                    UnboundedReceiverStream::new(rx)
                        .throttle(pool)
                        .for_each(|msg| {
                            global_tx.send((id, msg)).map_err(|e| error!("Sender error: {}", e)).ok();
                            future::ready(())
                        })
                        .await;
                });
                (id, tx)
            })
        })
        .collect::<HashMap<_, _>>();

    let intel = Intel::new(&CLIENT, USERNAME.as_deref(), PASSWORD.as_deref());

    if let Some(cookies) = &*COOKIES {
        intel
            .add_cookies(cookies.split("; ").filter_map(|cookie| {
                let (pos, _) = cookie.match_indices('=').next()?;
                Some((&cookie[0..pos], &cookie[(pos + 1)..]))
            }))
            .await;
    }

    intel.login().await.expect("Intel login failed");

    let (portal_tx, portal_rx) = mpsc::unbounded_channel();

    let conn = &conn;
    let config = &config;
    let intel = &intel;
    let senders = &senders;
    tokio::select! {
        _ = comm_survey(config, intel, senders, portal_tx.clone()) => {},
        _ = portal_survey(config, intel, senders) => {},
        _ = map_survey(conn, config, intel, senders, portal_tx) => {},
        _ = portal_scanner(conn, intel, portal_rx) => {},
    };
}

async fn get_portal_details_with_retry(
    intel: &Intel<'_>,
    portal_id: &str,
) -> Result<ingress_intel_rs::portal_details::IntelResponse, ingress_intel_rs::Error> {
    for i in 1..11 {
        match intel.get_portal_details(portal_id).await {
            Ok(portal) => return Ok(portal),
            Err(ingress_intel_rs::Error::Transport) | Err(ingress_intel_rs::Error::Status) if i < 10 => {
                debug!("get_portal_details({portal_id}) retry {i}");
                time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!()
}

async fn get_entities_around_with_retry(
    intel: &Intel<'_>,
    lat: f64,
    lon: f64,
) -> Result<Option<String>, ingress_intel_rs::Error> {
    for i in 1..11 {
        let res = match intel.get_entities_around(lat, lon, Some(15), None, None, None).await {
            Ok(res) => res,
            Err(err) => {
                error!("Error searching for portal on coordinates {lat},{lon} retry {i}: {err}");
                continue;
            }
        };

        match res
            .result
            .map
            .iter()
            .flat_map(|(_id, entity)| match entity {
                IntelResult::Error(_) => &[],
                IntelResult::Entities(e) => e.entities.as_slice(),
            })
            .find_map(|entity| {
                if entity.get_latitude().is_some_and(|l| (lat - l).abs() < 0.000001)
                    && entity.get_longitude().is_some_and(|l| (lon - l).abs() < 0.000001)
                {
                    entity.get_id()
                } else {
                    None
                }
            }) {
            Some(id) => return Ok(Some(id.to_owned())),
            None => {
                warn!(
                    "Can't find portal on coordinates {lat},{lon} retry {i} between {:?}",
                    res.result
                        .map
                        .into_values()
                        .map(|entity| match entity {
                            IntelResult::Error(err) => Err(err.error),
                            IntelResult::Entities(e) => Ok(e
                                .entities
                                .into_iter()
                                .filter_map(|entity| entity
                                    .get_id()
                                    .map(ToOwned::to_owned)
                                    .zip(entity.get_latitude())
                                    .zip(entity.get_longitude()))
                                .collect::<Vec<_>>()),
                        })
                        .collect::<Vec<_>>()
                );
                continue;
            }
        }
    }
    Ok(None)
}

#[derive(Debug)]
enum PortalOrCoords {
    Portal { id: String },
    Coords { lat: f64, lon: f64 },
}

async fn comm_survey(
    config: &config::Config,
    intel: &Intel<'static>,
    senders: &Senders,
    portal_tx: mpsc::UnboundedSender<(PortalOrCoords, i64)>,
) {
    let mut interval = time::interval(Duration::from_secs(30)); // main comm interval
    let mut sent_cache: Vec<LruCache<String, ()>> =
        vec![LruCache::with_expiry_duration(Duration::from_secs(120)); config.zones.len()]; //2 minutes cache
    loop {
        for (index, zone) in config.zones.iter().enumerate() {
            interval.tick().await;
            match intel
                .get_plexts(
                    zone.from,
                    zone.to,
                    Tab::All,
                    Some((Utc::now() - chrono::Duration::seconds(120)).timestamp_millis()),
                    None,
                )
                .await
            {
                Ok(res) => {
                    debug!("Got {} plexts", res.result.len());
                    if res.result.is_empty() {
                        continue;
                    }

                    let plexts = res
                        .result
                        .iter()
                        .rev()
                        .filter_map(|(_id, time, plext)| {
                            let msg_type = entities::PlextType::from(plext.plext.markup.as_slice());
                            let plext = entities::Plext::try_from((msg_type, &plext.plext, *time))
                                .map_err(|_| error!("Unable to create {:?} from {:?}", msg_type, plext.plext))
                                .ok()?;
                            if plext.should_scan() {
                                let _ = portal_tx.send((
                                    PortalOrCoords::Coords {
                                        lat: plext.get_lat().unwrap(),
                                        lon: plext.get_lon().unwrap(),
                                    },
                                    *time,
                                ));
                            }
                            Some(plext)
                        })
                        .collect::<Vec<_>>();
                    let msgs = dedup_flatten::windows_dedup_flatten(plexts.clone(), 8);
                    if res.result.len() != msgs.len() {
                        info!(
                            "Processed {} plexts: {:?}\nInto {} raw messages: {:?}\nInto {} messages: {:?}",
                            res.result.len(),
                            res.result,
                            plexts.len(),
                            plexts,
                            msgs.len(),
                            msgs
                        );
                    }

                    for msg in msgs.iter().filter(|m| !m.has_duplicates(&msgs)) {
                        if sent_cache[index].notify_insert(msg.to_string(), ()).0.is_none() {
                            for (id, filter) in &zone.users {
                                if filter.apply(msg) {
                                    senders[id]
                                        .send(Bot::Comm(msg.to_string()))
                                        .map_err(|e| error!("Sender error: {}", e))
                                        .ok();
                                } else {
                                    debug!("Filtered message for user {}: {:?}", id, msg);
                                }
                            }
                        } else {
                            debug!("Blocked duplicate entry {:?}", msg);
                        }
                    }
                }
                Err(ingress_intel_rs::Error::Deserialize) => {
                    if !SCANNING.load(Ordering::Relaxed) {
                        warn!("Probably rate limited, restart");
                        return;
                    }
                }
                _ => {}
            }
        }
    }
}

async fn portal_scanner<C>(
    conn: &C,
    intel: &Intel<'static>,
    mut portal_rx: mpsc::UnboundedReceiver<(PortalOrCoords, i64)>,
) where
    C: ConnectionTrait + TransactionTrait,
{
    while let Some((poc, time)) = portal_rx.recv().await {
        let id = match poc {
            PortalOrCoords::Portal { id } => id,
            PortalOrCoords::Coords { lat, lon } => match get_entities_around_with_retry(intel, lat, lon).await {
                Ok(Some(id)) => id,
                _ => continue,
            },
        };

        if let Ok(portal) = database::portal::Entity::get_latest_revision(conn, &id)
            .await
            .map_err(|err| error!("Portal {id} retrieve error: {err}"))
        {
            // scan portal only if informations are outdated
            if portal.is_none() || portal.as_ref().is_some_and(|portal| portal.timestamp < time) {
                if let Ok(res) = get_portal_details_with_retry(intel, &id)
                    .await
                    .map_err(|err| error!("Portal {id} scan error: {err}"))
                {
                    let _ = database::portal::update_or_insert(conn, &id, time, portal, res.result)
                        .await
                        .map_err(|err| error!("Portal {id} save error: {err}"));
                }
                // wait only if we have effectively tried to scan the portal
                time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

struct PortalCache {
    name: String,
    coords: (f64, f64),
    mods: Vec<Option<IntelMod>>,
    resonators: Vec<Option<IntelResonator>>,
}

impl From<IntelPortal> for PortalCache {
    fn from(p: IntelPortal) -> Self {
        PortalCache {
            name: p.get_title().to_owned(),
            coords: (p.get_latitude(), p.get_longitude()),
            mods: p.get_mods().to_vec(),
            resonators: p.get_resonators().to_vec(),
        }
    }
}

impl PortalCache {
    fn alarm(&self, other: &Self) -> Option<String> {
        let mut alarms = Vec::new();

        let old = self.get_mods_count();
        let new = other.get_mods_count();
        if old > new {
            alarms.push(format!("{} mods since last check ({} remaining)", old - new, new));
        }

        let old = self.get_resonators_count();
        let new = other.get_resonators_count();
        if old > new {
            alarms.push(format!("{} resonators since last check ({} remaining)", old - new, new));
        }

        let old = self.get_resonators_energy_sum();
        let new = other.get_resonators_energy_sum();
        if old > new {
            let max = self.get_resonators_max_energy_sum();
            let lost_perc = calc_perc(old - new, max);
            // if lost_perc != 15 || !self.all_resonators_lost_the_same(other, lost_perc) {
            let left_perc = calc_perc(new, max);
            alarms.push(format!("{}% of resonators energy since last check ({}% remaining)", lost_perc, left_perc));
            // }
        }

        if alarms.is_empty() {
            None
        } else {
            Some(format!(
                "{}Portal <a href=\"https://intel.ingress.com/intel?pll={},{}\">{}</a> lost:\n{}",
                symbols::ALERT,
                self.coords.0,
                self.coords.1,
                self.name,
                alarms.join("\n")
            ))
        }
    }

    fn get_mods_count(&self) -> usize {
        self.mods.iter().filter(|m| m.is_some()).count()
    }

    fn get_resonators_count(&self) -> usize {
        self.resonators.iter().filter(|r| r.is_some()).count()
    }

    fn get_resonators_energy_sum(&self) -> u16 {
        self.resonators.iter().filter_map(|r| r.as_ref().map(|r| r.get_energy())).sum()
    }

    fn get_resonators_max_energy_sum(&self) -> u16 {
        self.resonators.iter().filter_map(|r| r.as_ref().map(|r| get_portal_max_energy_by_level(r.get_level()))).sum()
    }

    // fn all_resonators_lost_the_same(&self, other: &Self, perc: u8) -> bool {
    //     self.resonators.iter().filter_map(|r| r.as_ref())
    //         .zip(other.resonators.iter().filter_map(|r| r.as_ref()))
    //         .all(|(old, new)| perc == calc_perc(old.get_energy() - new.get_energy(), get_portal_max_energy_by_level(old.get_level())))
    // }
}

fn calc_perc(diff: u16, sum: u16) -> u8 {
    ((diff as u32) * 100 / (sum as u32)) as u8
}

//should be moved into entities
fn get_portal_max_energy_by_level(level: u8) -> u16 {
    match level {
        1 => 1000,
        2 => 1500,
        3 => 2000,
        4 => 2500,
        5 => 3000,
        6 => 4000,
        7 => 5000,
        8 => 6000,
        _ => unreachable!(),
    }
}

async fn portal_survey(config: &config::Config, intel: &Intel<'static>, senders: &Senders) {
    let mut portals = HashMap::new();
    for zone in config.zones.iter() {
        for (id, filter) in &zone.users {
            if let Some(ps) = &filter.portals {
                for p in ps {
                    let entry = portals.entry(p.as_str()).or_insert_with(Vec::new);
                    entry.push(*id);
                }
            }
        }
    }
    let mut cache: HashMap<&str, PortalCache> = HashMap::with_capacity(portals.len());
    let mut interval = time::interval(Duration::from_secs(10)); // main portal interval
    loop {
        for (portal_id, users) in &portals {
            interval.tick().await;
            match get_portal_details_with_retry(intel, portal_id).await {
                Ok(res) => {
                    let new_cache = PortalCache::from(res.result);
                    if let Some(cached) = cache.get(portal_id) {
                        if let Some(msg) = cached.alarm(&new_cache) {
                            for user_id in users {
                                senders[user_id]
                                    .send(Bot::Portal(msg.clone()))
                                    .map_err(|e| error!("Sender error: {}", e))
                                    .ok();
                            }
                        }
                    }
                    cache.insert(*portal_id, new_cache);
                }
                Err(ingress_intel_rs::Error::Deserialize) => {
                    if !SCANNING.load(Ordering::Relaxed) {
                        warn!("Probably rate limited, restart");
                        return;
                    }
                }
                _ => {}
            }
        }
    }
}

async fn map_survey<C>(
    conn: &C,
    config: &config::Config,
    intel: &Intel<'static>,
    senders: &Senders,
    portal_tx: mpsc::UnboundedSender<(PortalOrCoords, i64)>,
) where
    C: ConnectionTrait,
{
    loop {
        let now = Utc::now().naive_utc();
        let next_trigger = now.date().and_hms_opt(0, 0, 0).expect("Midnight failed") + chrono::Duration::days(1);
        time::sleep((next_trigger - now).to_std().expect("Duration conversion failed")).await;
        SCANNING.store(true, Ordering::Relaxed);
        for zone in config.zones.iter() {
            let from_lat = zone.from[0] as f64 / 1000000_f64;
            let from_lng = zone.from[1] as f64 / 1000000_f64;
            let to_lat = zone.to[0] as f64 / 1000000_f64;
            let to_lng = zone.to[1] as f64 / 1000000_f64;
            if let Ok(entities) = intel
                .get_entities_in_range(
                    (from_lat, from_lng),
                    (to_lat, to_lng),
                    Some(15),
                    Some(7),
                    None,
                    None,
                    (1, Duration::from_secs(1)),
                )
                .await
            {
                let mut list = HashMap::new();
                for portal in entities.iter().flat_map(|e| &e.entities) {
                    if !portal.is_portal() {
                        continue;
                    }

                    if database::portal::should_scan(conn, portal).await {
                        let _ = portal_tx.send((
                            PortalOrCoords::Portal { id: portal.get_id().unwrap().to_owned() },
                            portal.get_timestamp().unwrap() as i64,
                        ));
                    }

                    if let (Some(name), Some(level), Some(faction), Some(lat), Some(lon)) = (
                        portal.get_name(),
                        portal.get_level(),
                        portal.get_faction(),
                        portal.get_latitude().and_then(Decimal::from_f64),
                        portal.get_longitude().and_then(Decimal::from_f64),
                    ) {
                        if level < 7 || faction.is_machina() {
                            continue;
                        }
                        let sublist = list.entry((level, faction)).or_insert_with(Vec::new);
                        sublist.push(entities::Portal { name, address: "maps", lat, lon });
                    }
                }
                if !list.is_empty() {
                    // for each portal, find other same-faction portals distances
                    let distances = list
                        .iter()
                        .flat_map(|((_, faction), sublist)| sublist.iter().map(move |p| (faction, p)))
                        .map(|(f1, p1)| {
                            (
                                (*f1, p1.lat, p1.lon),
                                list.iter()
                                    .filter(|((_, f2), _)| f1 == f2)
                                    .flat_map(|(_, sublist)| {
                                        sublist.iter().flat_map(|p2| {
                                            let dist = calc_dist((p1.lat, p1.lon), (p2.lat, p2.lon))?;
                                            // limit to 1 Km distances
                                            (dist < 1.0).then_some(dist)
                                        })
                                    })
                                    .fold((0, 0.0), |(count, sum), dist| (count + 1, sum + dist)),
                            )
                        })
                        .fold(HashMap::new(), |mut acc, ((f, lat, lon), t)| {
                            let faction: &mut HashMap<_, _> = acc.entry(f).or_default();
                            faction.insert((lat, lon), t);
                            acc
                        });
                    let biggest_cluster = distances
                        .iter()
                        .map(|(f, v)| (*f, v.values().map(|t| t.0).max().expect("Cluster max failed")))
                        .collect::<HashMap<_, _>>();
                    let min_distance = distances
                        .iter()
                        .map(|(f, v)| {
                            (
                                *f,
                                v.values()
                                    .filter(|t| t.0 == biggest_cluster[f])
                                    .map(|t| t.1)
                                    .min_by(|a, b| a.partial_cmp(b).expect("Distance compare failed"))
                                    .expect("Distance min failed"),
                            )
                        })
                        .collect::<HashMap<_, _>>();

                    for ((level, faction), mut sublist) in list {
                        sublist.sort_unstable_by(|p1, p2| p1.name.cmp(p2.name));

                        // split messages to respect 4094 bytes message limit
                        let init = format!("{} L{level} portal:", entities::Team::from(faction));
                        let msgs = sublist.into_iter().fold(vec![init.clone()], |mut msgs, portal| {
                            let msg = portal.to_string();
                            let mut slot = msgs.len() - 1;
                            if msgs[slot].len() + msg.len() + 3 > 4094 {
                                msgs.push(init.clone());
                                slot += 1;
                            }
                            if distances[&faction].get(&(portal.lat, portal.lon))
                                == Some(&(biggest_cluster[&faction], min_distance[&faction]))
                            {
                                msgs[slot].push_str("\n> ");
                            } else {
                                msgs[slot].push_str("\n* ");
                            }
                            msgs[slot].push_str(&msg);
                            msgs
                        });

                        // send messages
                        for (id, conf) in &zone.users {
                            if conf.resume.unwrap_or_default() {
                                for msg in &msgs {
                                    senders[id]
                                        .send(Bot::Portal(msg.clone()))
                                        .map_err(|e| error!("Sender error: {}", e))
                                        .ok();
                                }
                            }
                        }
                    }
                }
            }
        }
        SCANNING.store(false, Ordering::Relaxed);
    }
}

fn calc_dist((lat_from, lon_from): (Decimal, Decimal), (lat_to, lon_to): (Decimal, Decimal)) -> Option<f64> {
    let lat_from = lat_from.to_f64()?.to_radians();
    let lon_from = lon_from.to_f64()?.to_radians();
    let lat_to = lat_to.to_f64()?.to_radians();
    let lon_to = lon_to.to_f64()?.to_radians();

    let lat_delta = lat_to - lat_from;
    let lon_delta = lon_to - lon_from;

    let angle = 2_f64
        * ((lat_delta / 2_f64).sin().powi(2) + lat_from.cos() * lat_to.cos() * (lon_delta / 2_f64).sin().powi(2))
            .sqrt()
            .asin();
    Some(angle * 6371_f64)
}

#[cfg(test)]
mod tests {
    use tracing::{debug, info};

    #[tokio::test]
    async fn filter() {
        tracing_subscriber::fmt::try_init().ok();
        let inputs = [
            r#"{"result":[["f3aec224b4f8425d8c5733b63746d5f5.d",1642338384866,{"plext":{"text":"alepro95 deployed a Resonator on Padova - Cat & Butterflies (Via Roma, 71, 35123 Padua, Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Padova - Cat & Butterflies (Via Roma, 71, 35123 Padua, Padua, Italy)","name":"Padova - Cat & Butterflies","address":"Via Roma, 71, 35123 Padua, Padua, Italy","latE6":45404672,"lngE6":11875960,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["7c2e71fd947846bbb277323d1580556a.d",1642338384866,{"plext":{"text":"alepro95 captured Padova - Cat & Butterflies (Via Roma, 71, 35123 Padua, Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Padova - Cat & Butterflies (Via Roma, 71, 35123 Padua, Padua, Italy)","name":"Padova - Cat & Butterflies","address":"Via Roma, 71, 35123 Padua, Padua, Italy","latE6":45404672,"lngE6":11875960,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["6281366404864f77b03ad1bf83eab85e.d",1642338382421,{"plext":{"text":"alepro95 captured Chiesa Santa Maria Dei Servi (Via Roma, 66, Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Chiesa Santa Maria Dei Servi (Via Roma, 66, Padua, Italy)","name":"Chiesa Santa Maria Dei Servi","address":"Via Roma, 66, Padua, Italy","latE6":45404533,"lngE6":11875888,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["09cdb5a600e647f384862b5e0a68ffbf.d",1642338382421,{"plext":{"text":"alepro95 deployed a Resonator on Chiesa Santa Maria Dei Servi (Via Roma, 66, Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Chiesa Santa Maria Dei Servi (Via Roma, 66, Padua, Italy)","name":"Chiesa Santa Maria Dei Servi","address":"Via Roma, 66, Padua, Italy","latE6":45404533,"lngE6":11875888,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["ae8464aea3df4617a5fc00d09ec61db3.d",1642338358435,{"plext":{"text":"alepro95 deployed a Resonator on Targa a Paolo Sarpi (Via Roma, 92, 35122 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Targa a Paolo Sarpi (Via Roma, 92, 35122 Padua, Italy)","name":"Targa a Paolo Sarpi","address":"Via Roma, 92, 35122 Padua, Italy","latE6":45404259,"lngE6":11875774,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["0a6380e466f248e59d7daef5750c8153.d",1642338358435,{"plext":{"text":"alepro95 captured Targa a Paolo Sarpi (Via Roma, 92, 35122 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Targa a Paolo Sarpi (Via Roma, 92, 35122 Padua, Italy)","name":"Targa a Paolo Sarpi","address":"Via Roma, 92, 35122 Padua, Italy","latE6":45404259,"lngE6":11875774,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["84b69722b04f46249c976c4f8281315c.d",1642338309537,{"plext":{"text":"alepro95 deployed a Resonator on Teatro Ruzante (Riviera Tito Livio, 45, 35123 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Teatro Ruzante (Riviera Tito Livio, 45, 35123 Padua, Italy)","name":"Teatro Ruzante","address":"Riviera Tito Livio, 45, 35123 Padua, Italy","latE6":45404103,"lngE6":11876734,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["5d73c9da48f84e2a8c6ea3ed41881ea3.d",1642338309537,{"plext":{"text":"alepro95 captured Teatro Ruzante (Riviera Tito Livio, 45, 35123 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Teatro Ruzante (Riviera Tito Livio, 45, 35123 Padua, Italy)","name":"Teatro Ruzante","address":"Riviera Tito Livio, 45, 35123 Padua, Italy","latE6":45404103,"lngE6":11876734,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["e9e922894a134f3b8b8455a6936d0247.d",1642338305616,{"plext":{"text":"alepro95 deployed a Resonator on Istituto Dante Alighieri Padova (Riviera Tito Livio, 43, 35123 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Istituto Dante Alighieri Padova (Riviera Tito Livio, 43, 35123 Padua, Italy)","name":"Istituto Dante Alighieri Padova","address":"Riviera Tito Livio, 43, 35123 Padua, Italy","latE6":45404416,"lngE6":11876862,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["733ba6ec863c4d65a2f3aec13f93aa20.d",1642338305616,{"plext":{"text":"alepro95 captured Istituto Dante Alighieri Padova (Riviera Tito Livio, 43, 35123 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Istituto Dante Alighieri Padova (Riviera Tito Livio, 43, 35123 Padua, Italy)","name":"Istituto Dante Alighieri Padova","address":"Riviera Tito Livio, 43, 35123 Padua, Italy","latE6":45404416,"lngE6":11876862,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["bb8a06df1a664d7db737b8a3fdc15731.d",1642338106025,{"plext":{"text":"alepro95 deployed a Resonator on Targa a Silvio Trentin (Via del Santo, 64, 35123 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Targa a Silvio Trentin (Via del Santo, 64, 35123 Padua, Italy)","name":"Targa a Silvio Trentin","address":"Via del Santo, 64, 35123 Padua, Italy","latE6":45403025,"lngE6":11879336,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["ace0238f441149bfb8a59de0e4f268bd.d",1642338106025,{"plext":{"text":"alepro95 captured Targa a Silvio Trentin (Via del Santo, 64, 35123 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Targa a Silvio Trentin (Via del Santo, 64, 35123 Padua, Italy)","name":"Targa a Silvio Trentin","address":"Via del Santo, 64, 35123 Padua, Italy","latE6":45403025,"lngE6":11879336,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["92e6ab2b0a8a441cac7c8b13623d7936.d",1642338103159,{"plext":{"text":"alepro95 captured Dimora di Giovanni Prati - Targa (Via del Santo, 42, 35123 Padova PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Dimora di Giovanni Prati - Targa (Via del Santo, 42, 35123 Padova PD, Italy)","name":"Dimora di Giovanni Prati - Targa","address":"Via del Santo, 42, 35123 Padova PD, Italy","latE6":45403650,"lngE6":11879205,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["6b473bb84de14fd6878ee186dd610a41.d",1642338103159,{"plext":{"text":"alepro95 deployed a Resonator on Dimora di Giovanni Prati - Targa (Via del Santo, 42, 35123 Padova PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Dimora di Giovanni Prati - Targa (Via del Santo, 42, 35123 Padova PD, Italy)","name":"Dimora di Giovanni Prati - Targa","address":"Via del Santo, 42, 35123 Padova PD, Italy","latE6":45403650,"lngE6":11879205,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["e5373894456f4d50aa9778546a7d07e1.d",1642338098612,{"plext":{"text":"alepro95 deployed a Resonator on Ufficio Postale via Rudena (Via Rudena, 89, 35123 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Ufficio Postale via Rudena (Via Rudena, 89, 35123 Padua, Italy)","name":"Ufficio Postale via Rudena","address":"Via Rudena, 89, 35123 Padua, Italy","latE6":45403202,"lngE6":11879104,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["b90939613f4c4ae892f4fe84ea69183e.d",1642338098612,{"plext":{"text":"alepro95 captured Ufficio Postale via Rudena (Via Rudena, 89, 35123 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Ufficio Postale via Rudena (Via Rudena, 89, 35123 Padua, Italy)","name":"Ufficio Postale via Rudena","address":"Via Rudena, 89, 35123 Padua, Italy","latE6":45403202,"lngE6":11879104,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["a04797076d5748b980064981aa2f1053.d",1642338048298,{"plext":{"text":"alepro95 captured Aula Studio Galilei (Via Galileo Galilei, 42, 35121 Padova PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Aula Studio Galilei (Via Galileo Galilei, 42, 35121 Padova PD, Italy)","name":"Aula Studio Galilei","address":"Via Galileo Galilei, 42, 35121 Padova PD, Italy","latE6":45403490,"lngE6":11879937,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["398e1d87302d4d1686abc790e0a9ac0b.d",1642338048298,{"plext":{"text":"alepro95 deployed a Resonator on Aula Studio Galilei (Via Galileo Galilei, 42, 35121 Padova PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Aula Studio Galilei (Via Galileo Galilei, 42, 35121 Padova PD, Italy)","name":"Aula Studio Galilei","address":"Via Galileo Galilei, 42, 35121 Padova PD, Italy","latE6":45403490,"lngE6":11879937,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["ed5fdb88fc954f9781b8ccc863e21b88.d",1642338009941,{"plext":{"text":"alepro95 deployed a Resonator on Decorazione muraria (Via Galileo Galilei, 24-40, Padua, Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Decorazione muraria (Via Galileo Galilei, 24-40, Padua, Padua, Italy)","name":"Decorazione muraria","address":"Via Galileo Galilei, 24-40, Padua, Padua, Italy","latE6":45403470,"lngE6":11880513,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["e6ee72f8bee540cf8c0c3d66efdb8d45.d",1642338009941,{"plext":{"text":"alepro95 captured Decorazione muraria (Via Galileo Galilei, 24-40, Padua, Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Decorazione muraria (Via Galileo Galilei, 24-40, Padua, Padua, Italy)","name":"Decorazione muraria","address":"Via Galileo Galilei, 24-40, Padua, Padua, Italy","latE6":45403470,"lngE6":11880513,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["e7e39f79ba754ba0a7cbddba351abdd9.d",1642337966039,{"plext":{"text":"alepro95 captured Statue Cuamm (Via Galileo Galilei, 22-56, Padua, Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Statue Cuamm (Via Galileo Galilei, 22-56, Padua, Padua, Italy)","name":"Statue Cuamm","address":"Via Galileo Galilei, 22-56, Padua, Padua, Italy","latE6":45403444,"lngE6":11881296,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["29b959549dc24520a3d0c628a739fb03.d",1642337966039,{"plext":{"text":"alepro95 deployed a Resonator on Statue Cuamm (Via Galileo Galilei, 22-56, Padua, Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Statue Cuamm (Via Galileo Galilei, 22-56, Padua, Padua, Italy)","name":"Statue Cuamm","address":"Via Galileo Galilei, 22-56, Padua, Padua, Italy","latE6":45403444,"lngE6":11881296,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["eca07cd7b247459e800757269d57b058.d",1642337934611,{"plext":{"text":"alepro95 captured La casa di Galileo Galilei - Targa (Via Galileo Galilei, 15, 35123 Padova PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"La casa di Galileo Galilei - Targa (Via Galileo Galilei, 15, 35123 Padova PD, Italy)","name":"La casa di Galileo Galilei - Targa","address":"Via Galileo Galilei, 15, 35123 Padova PD, Italy","latE6":45403444,"lngE6":11881871,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["3050f962c5e04808b16855c94508f181.d",1642337934611,{"plext":{"text":"alepro95 deployed a Resonator on La casa di Galileo Galilei - Targa (Via Galileo Galilei, 15, 35123 Padova PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"La casa di Galileo Galilei - Targa (Via Galileo Galilei, 15, 35123 Padova PD, Italy)","name":"La casa di Galileo Galilei - Targa","address":"Via Galileo Galilei, 15, 35123 Padova PD, Italy","latE6":45403444,"lngE6":11881871,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["99b7c5e47e45486ebe8c8a0bb5ab8a28.d",1642337836339,{"plext":{"text":"alepro95 deployed a Resonator on Testa Giullare (Via San Francesco, 129, Padua, Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Testa Giullare (Via San Francesco, 129, Padua, Padua, Italy)","name":"Testa Giullare","address":"Via San Francesco, 129, Padua, Padua, Italy","latE6":45402962,"lngE6":11883078,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["d08ee5e61b5e4a89adc307201a54cff9.d",1642337799306,{"plext":{"text":"alepro95 captured Affresco Sottoportico Ponte Corvo (Via San Francesco, 179, 35121 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Affresco Sottoportico Ponte Corvo (Via San Francesco, 179, 35121 Padua, Italy)","name":"Affresco Sottoportico Ponte Corvo","address":"Via San Francesco, 179, 35121 Padua, Italy","latE6":45402291,"lngE6":11883294,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["877f18f62a8940f4bc4b32c50cb9feae.d",1642337799306,{"plext":{"text":"alepro95 deployed a Resonator on Affresco Sottoportico Ponte Corvo (Via San Francesco, 179, 35121 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Affresco Sottoportico Ponte Corvo (Via San Francesco, 179, 35121 Padua, Italy)","name":"Affresco Sottoportico Ponte Corvo","address":"Via San Francesco, 179, 35121 Padua, Italy","latE6":45402291,"lngE6":11883294,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["c4f581cf77024146ad149753e5346de7.d",1642337794335,{"plext":{"text":"farfaska created a Control Field @Al Donatore (Corso del Donatore, 14, 30029 San Stino di Livenza VE, Italy) +18 MUs","team":"RESISTANCE","markup":[["PLAYER",{"plain":"farfaska","team":"RESISTANCE"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Al Donatore (Corso del Donatore, 14, 30029 San Stino di Livenza VE, Italy)","name":"Al Donatore","address":"Corso del Donatore, 14, 30029 San Stino di Livenza VE, Italy","latE6":45730341,"lngE6":12679786,"team":"RESISTANCE"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"18"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["ba4858c452ad4ea6ae6eeabf4d0e73af.d",1642337794335,{"plext":{"text":"farfaska linked Al Donatore (Corso del Donatore, 14, 30029 San Stino di Livenza VE, Italy) to Mappa Bosco Triestina (Via I\u00b0 Maggio, 2, 30029 San Stino di Livenza VE, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"farfaska","team":"RESISTANCE"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Al Donatore (Corso del Donatore, 14, 30029 San Stino di Livenza VE, Italy)","name":"Al Donatore","address":"Corso del Donatore, 14, 30029 San Stino di Livenza VE, Italy","latE6":45730341,"lngE6":12679786,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Mappa Bosco Triestina (Via I\u00b0 Maggio, 2, 30029 San Stino di Livenza VE, Italy)","name":"Mappa Bosco Triestina","address":"Via I\u00b0 Maggio, 2, 30029 San Stino di Livenza VE, Italy","latE6":45714841,"lngE6":12691849,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["23b7eee4ae1247839d26f6a4d7fb9976.d",1642337793313,{"plext":{"text":"farfaska linked Al Donatore (Corso del Donatore, 14, 30029 San Stino di Livenza VE, Italy) to Monumento Ai Bersaglieri Di San Stino Di Livenza (Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"farfaska","team":"RESISTANCE"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Al Donatore (Corso del Donatore, 14, 30029 San Stino di Livenza VE, Italy)","name":"Al Donatore","address":"Corso del Donatore, 14, 30029 San Stino di Livenza VE, Italy","latE6":45730341,"lngE6":12679786,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Monumento Ai Bersaglieri Di San Stino Di Livenza (Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy)","name":"Monumento Ai Bersaglieri Di San Stino Di Livenza","address":"Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy","latE6":45728173,"lngE6":12678652,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["02de5027f55d4968b68ab839bb5aa862.d",1642337783635,{"plext":{"text":"Maravea linked Anconetta di San Giovanni Napomuceno (Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy) to Memorial Maura Moro Pinzan (Salita di Gretta, 34070 Savogna D'Isonzo GO, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Maravea","team":"ENLIGHTENED"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Anconetta di San Giovanni Napomuceno (Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy)","name":"Anconetta di San Giovanni Napomuceno","address":"Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy","latE6":45892385,"lngE6":13502258,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Memorial Maura Moro Pinzan (Salita di Gretta, 34070 Savogna D'Isonzo GO, Italy)","name":"Memorial Maura Moro Pinzan","address":"Salita di Gretta, 34070 Savogna D'Isonzo GO, Italy","latE6":45874852,"lngE6":13554145,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["bc553f4c0ae445babc22e8e7afdbc968.d",1642337777824,{"plext":{"text":"Maravea linked Anconetta di San Giovanni Napomuceno (Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy) to Monumento ai caduti della resistenza (Coti\u010di, 34078 San Michele del Carso Gorizia, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Maravea","team":"ENLIGHTENED"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Anconetta di San Giovanni Napomuceno (Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy)","name":"Anconetta di San Giovanni Napomuceno","address":"Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy","latE6":45892385,"lngE6":13502258,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Monumento ai caduti della resistenza (Coti\u010di, 34078 San Michele del Carso Gorizia, Italy)","name":"Monumento ai caduti della resistenza","address":"Coti\u010di, 34078 San Michele del Carso Gorizia, Italy","latE6":45879502,"lngE6":13559056,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["aa230c7537c041b6aa19ecff5808f00a.d",1642337770897,{"plext":{"text":"Maravea linked Anconetta di San Giovanni Napomuceno (Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy) to Fortezza Nel Bosco Di Doberd\u00f2 Del Lago (Via Bonetti, 34070 Doberdo' del lago GO, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Maravea","team":"ENLIGHTENED"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Anconetta di San Giovanni Napomuceno (Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy)","name":"Anconetta di San Giovanni Napomuceno","address":"Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy","latE6":45892385,"lngE6":13502258,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Fortezza Nel Bosco Di Doberd\u00f2 Del Lago (Via Bonetti, 34070 Doberdo' del lago GO, Italy)","name":"Fortezza Nel Bosco Di Doberd\u00f2 Del Lago","address":"Via Bonetti, 34070 Doberdo' del lago GO, Italy","latE6":45841840,"lngE6":13550149,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["63fc318c703647da97e241d7dc24b329.d",1642337752030,{"plext":{"text":"alepro95 deployed a Resonator on Targa a Giampaolo Prof. Vlacovich (Via Melchiorre Cesarotti, 21, 35123 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Targa a Giampaolo Prof. Vlacovich (Via Melchiorre Cesarotti, 21, 35123 Padua, Italy)","name":"Targa a Giampaolo Prof. Vlacovich","address":"Via Melchiorre Cesarotti, 21, 35123 Padua, Italy","latE6":45402295,"lngE6":11882764,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["37e4fa6c258f4fc398148d6fa228e8d0.d",1642337733255,{"plext":{"text":"alepro95 deployed a Resonator on Targa a Melchiorre Cesarotti (Via Melchiorre Cesarotti, 21, 35123 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Targa a Melchiorre Cesarotti (Via Melchiorre Cesarotti, 21, 35123 Padua, Italy)","name":"Targa a Melchiorre Cesarotti","address":"Via Melchiorre Cesarotti, 21, 35123 Padua, Italy","latE6":45402194,"lngE6":11882513,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["ef0880165bd14bc0a63fc49c898d73e5.d",1642337708026,{"plext":{"text":"alepro95 deployed a Resonator on Padova I Putti (Via Melchiorre Cesarotti, 21, 35123 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Padova I Putti (Via Melchiorre Cesarotti, 21, 35123 Padua, Italy)","name":"Padova I Putti","address":"Via Melchiorre Cesarotti, 21, 35123 Padua, Italy","latE6":45402238,"lngE6":11882121,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["7765c47fc9bd4aab8df91454477f11cc.d",1642337680059,{"plext":{"text":"alepro95 deployed a Resonator on Padova - Basilica del Santo (Lato Via Cesarotti) (Via Melchiorre Cesarotti, 35123 Padua, Province of Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Padova - Basilica del Santo (Lato Via Cesarotti) (Via Melchiorre Cesarotti, 35123 Padua, Province of Padua, Italy)","name":"Padova - Basilica del Santo (Lato Via Cesarotti)","address":"Via Melchiorre Cesarotti, 35123 Padua, Province of Padua, Italy","latE6":45401849,"lngE6":11881077,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["c229ed28bc1f407a9a199c915506d450.d",1642337647987,{"plext":{"text":"Morwen98 deployed a Resonator on Padova - Basilica del Santo (Lato Via Cesarotti) (Via Melchiorre Cesarotti, 35123 Padua, Province of Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"Morwen98","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Padova - Basilica del Santo (Lato Via Cesarotti) (Via Melchiorre Cesarotti, 35123 Padua, Province of Padua, Italy)","name":"Padova - Basilica del Santo (Lato Via Cesarotti)","address":"Via Melchiorre Cesarotti, 35123 Padua, Province of Padua, Italy","latE6":45401849,"lngE6":11881077,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["f6baacfeec8d4521a77ac20a89ac9bcb.d",1642337647191,{"plext":{"text":"alepro95 deployed a Resonator on Caserma del Santo (Via Melchiorre Cesarotti, 35123 Padua, Province of Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Caserma del Santo (Via Melchiorre Cesarotti, 35123 Padua, Province of Padua, Italy)","name":"Caserma del Santo","address":"Via Melchiorre Cesarotti, 35123 Padua, Province of Padua, Italy","latE6":45402076,"lngE6":11881396,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["98af0b8372b745ac94d40b22d186b927.d",1642337647191,{"plext":{"text":"alepro95 captured Caserma del Santo (Via Melchiorre Cesarotti, 35123 Padua, Province of Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Caserma del Santo (Via Melchiorre Cesarotti, 35123 Padua, Province of Padua, Italy)","name":"Caserma del Santo","address":"Via Melchiorre Cesarotti, 35123 Padua, Province of Padua, Italy","latE6":45402076,"lngE6":11881396,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["33c15c3f62f141f48acb511a949b4449.d",1642337639133,{"plext":{"text":"Morwen98 deployed a Resonator on S. Antonio (Piazza del Santo, 35123 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"Morwen98","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"S. Antonio (Piazza del Santo, 35123 Padua, Italy)","name":"S. Antonio","address":"Piazza del Santo, 35123 Padua, Italy","latE6":45401752,"lngE6":11881264,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["98587a40ca464e58adcf177beed383c9.d",1642337562389,{"plext":{"text":"alepro95 deployed a Resonator on Sant'Antonio Capitello (Via del Santo, 106, 35123 Padua, Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Sant'Antonio Capitello (Via del Santo, 106, 35123 Padua, Padua, Italy)","name":"Sant'Antonio Capitello","address":"Via del Santo, 106, 35123 Padua, Padua, Italy","latE6":45401910,"lngE6":11879833,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["5e2399588bd243b5965f7821f0cae1ea.d",1642337562389,{"plext":{"text":"alepro95 captured Sant'Antonio Capitello (Via del Santo, 106, 35123 Padua, Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"alepro95","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Sant'Antonio Capitello (Via del Santo, 106, 35123 Padua, Padua, Italy)","name":"Sant'Antonio Capitello","address":"Via del Santo, 106, 35123 Padua, Padua, Italy","latE6":45401910,"lngE6":11879833,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["cc6d810e6cbf4642a1d181610d78dbd3.d",1642337561216,{"plext":{"text":"Maravea created a Control Field @Anconetta di San Giovanni Napomuceno (Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy) +11 MUs","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Maravea","team":"ENLIGHTENED"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Anconetta di San Giovanni Napomuceno (Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy)","name":"Anconetta di San Giovanni Napomuceno","address":"Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy","latE6":45892385,"lngE6":13502258,"team":"ENLIGHTENED"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"11"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["34e77608f1ff418ebace323ea43173f3.d",1642337561216,{"plext":{"text":"Maravea linked Anconetta di San Giovanni Napomuceno (Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy) to Bassorilievo Industriale (Via Giuseppe Garibaldi, 73, 34072 Gradisca d'Isonzo GO, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Maravea","team":"ENLIGHTENED"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Anconetta di San Giovanni Napomuceno (Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy)","name":"Anconetta di San Giovanni Napomuceno","address":"Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy","latE6":45892385,"lngE6":13502258,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Bassorilievo Industriale (Via Giuseppe Garibaldi, 73, 34072 Gradisca d'Isonzo GO, Italy)","name":"Bassorilievo Industriale","address":"Via Giuseppe Garibaldi, 73, 34072 Gradisca d'Isonzo GO, Italy","latE6":45890232,"lngE6":13496101,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["178e3eedd82a4efcb94c1a425aa18422.d",1642337555428,{"plext":{"text":"Maravea linked Anconetta di San Giovanni Napomuceno (Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy) to Colonna Romana (Via Roma, 5, 34072 Gradisca D'Isonzo Gorizia, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Maravea","team":"ENLIGHTENED"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Anconetta di San Giovanni Napomuceno (Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy)","name":"Anconetta di San Giovanni Napomuceno","address":"Via Gorizia, 21-23, 34072 Gradisca D'Isonzo Province of Gorizia, Italy","latE6":45892385,"lngE6":13502258,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Colonna Romana (Via Roma, 5, 34072 Gradisca D'Isonzo Gorizia, Italy)","name":"Colonna Romana","address":"Via Roma, 5, 34072 Gradisca D'Isonzo Gorizia, Italy","latE6":45892699,"lngE6":13499664,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["e1f9106f4ca741aea58a77f29f02c171.d",1642337533732,{"plext":{"text":"farfaska created a Control Field @Monumento Ai Bersaglieri Di San Stino Di Livenza (Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy) +15 MUs","team":"RESISTANCE","markup":[["PLAYER",{"plain":"farfaska","team":"RESISTANCE"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Monumento Ai Bersaglieri Di San Stino Di Livenza (Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy)","name":"Monumento Ai Bersaglieri Di San Stino Di Livenza","address":"Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy","latE6":45728173,"lngE6":12678652,"team":"RESISTANCE"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"15"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["ada22ea196f741d88130e0b41f1e823c.d",1642337533732,{"plext":{"text":"farfaska linked Monumento Ai Bersaglieri Di San Stino Di Livenza (Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy) to Mappa Bosco Triestina (Via I\u00b0 Maggio, 2, 30029 San Stino di Livenza VE, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"farfaska","team":"RESISTANCE"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Monumento Ai Bersaglieri Di San Stino Di Livenza (Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy)","name":"Monumento Ai Bersaglieri Di San Stino Di Livenza","address":"Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy","latE6":45728173,"lngE6":12678652,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Mappa Bosco Triestina (Via I\u00b0 Maggio, 2, 30029 San Stino di Livenza VE, Italy)","name":"Mappa Bosco Triestina","address":"Via I\u00b0 Maggio, 2, 30029 San Stino di Livenza VE, Italy","latE6":45714841,"lngE6":12691849,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["f65b92d52de84bb29c35a28d6c179db9.d",1642337532670,{"plext":{"text":"farfaska linked Monumento Ai Bersaglieri Di San Stino Di Livenza (Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy) to Parco di Educazione Stradale (Via S. Tommaso Moro, 26, 30029 San Stino di Livenza VE, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"farfaska","team":"RESISTANCE"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Monumento Ai Bersaglieri Di San Stino Di Livenza (Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy)","name":"Monumento Ai Bersaglieri Di San Stino Di Livenza","address":"Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy","latE6":45728173,"lngE6":12678652,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Parco di Educazione Stradale (Via S. Tommaso Moro, 26, 30029 San Stino di Livenza VE, Italy)","name":"Parco di Educazione Stradale","address":"Via S. Tommaso Moro, 26, 30029 San Stino di Livenza VE, Italy","latE6":45726484,"lngE6":12677666,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["6d32b20bcde6430d8ba67ab47707bcfa.d",1642337516649,{"plext":{"text":"farfaska captured Monumento Ai Bersaglieri Di San Stino Di Livenza (Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"farfaska","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Monumento Ai Bersaglieri Di San Stino Di Livenza (Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy)","name":"Monumento Ai Bersaglieri Di San Stino Di Livenza","address":"Via Alcide De Gasperi, 20, 30029 San Stino di Livenza VE, Italy","latE6":45728173,"lngE6":12678652,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}]]}"#,
        ];
        for s in inputs {
            let res: ingress_intel_rs::plexts::IntelResponse = serde_json::from_str(s).unwrap();
            let config = crate::config::get().await.unwrap();

            let plexts = res
                .result
                .iter()
                .rev()
                .filter_map(|(_id, time, plext)| {
                    let msg_type = crate::entities::PlextType::from(plext.plext.markup.as_slice());
                    crate::entities::Plext::try_from((msg_type, &plext.plext, *time))
                        .map_err(|_| tracing::error!("{msg_type:?}\nOriginal plext markups: {:#?}", plext.plext.markup))
                        .ok()
                })
                .collect::<Vec<_>>();
            let msgs = crate::dedup_flatten::windows_dedup_flatten(plexts, 8);
            for (index, zone) in config.zones.iter().enumerate() {
                for msg in msgs.iter().filter(|m| !m.has_duplicates(&msgs)) {
                    for (id, filter) in &zone.users {
                        if filter.apply(msg) {
                            info!("Sent message to user {} in zone {}: {:?}", id, index, msg);
                        } else {
                            debug!("Filtered message for user {} in zone {}: {:?}", id, index, msg);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn plexts() {
        let s = r#"{"result":[["950163b7358548ff8d0f642a3d4876a6.d",1719762782388,{"plext":{"text":"_\u0336\u0331\u030d_\u0334\u0333\u0349\u0306\u0308\u0301M\u0337\u0354\u0324\u0352\u0104\u0337\u030dC\u0334\u033c\u0315\u0345H\u0336\u0339\u0355\u033c\u033e\u1e2c\u0335\u0307\u033e\u0313N\u0335\u033a\u0355\u0352\u0300\u030d\u00c4\u0334\u031e\u0330\u0301_\u0334\u0326\u0300\u0346\u0313_\u0337\u0323\u0308\u0301 linked A.M.G. Judo Murano (Fondamenta Antonio Colleoni, 14, 30141 Venezia VE, Italy) to Poste Italiane Murano LY (Fondamenta Antonio Maschio, 47-48, 30141 Venezia VE, Italy)","team":"NEUTRAL","markup":[["PLAYER",{"plain":"_\u0336\u0331\u030d_\u0334\u0333\u0349\u0306\u0308\u0301M\u0337\u0354\u0324\u0352\u0104\u0337\u030dC\u0334\u033c\u0315\u0345H\u0336\u0339\u0355\u033c\u033e\u1e2c\u0335\u0307\u033e\u0313N\u0335\u033a\u0355\u0352\u0300\u030d\u00c4\u0334\u031e\u0330\u0301_\u0334\u0326\u0300\u0346\u0313_\u0337\u0323\u0308\u0301","team":"NEUTRAL"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"A.M.G. Judo Murano (Fondamenta Antonio Colleoni, 14, 30141 Venezia VE, Italy)","name":"A.M.G. Judo Murano","address":"Fondamenta Antonio Colleoni, 14, 30141 Venezia VE, Italy","latE6":45455210,"lngE6":12354014,"team":"NEUTRAL"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Poste Italiane Murano LY (Fondamenta Antonio Maschio, 47-48, 30141 Venezia VE, Italy)","name":"Poste Italiane Murano LY","address":"Fondamenta Antonio Maschio, 47-48, 30141 Venezia VE, Italy","latE6":45455478,"lngE6":12356645,"team":"NEUTRAL"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["37b2d529f7de4ba6a03631fe9618f902.d",1719762782388,{"plext":{"text":"_\u0336\u0331\u030d_\u0334\u0333\u0349\u0306\u0308\u0301M\u0337\u0354\u0324\u0352\u0104\u0337\u030dC\u0334\u033c\u0315\u0345H\u0336\u0339\u0355\u033c\u033e\u1e2c\u0335\u0307\u033e\u0313N\u0335\u033a\u0355\u0352\u0300\u030d\u00c4\u0334\u031e\u0330\u0301_\u0334\u0326\u0300\u0346\u0313_\u0337\u0323\u0308\u0301 linked A.M.G. Judo Murano (Fondamenta Antonio Colleoni, 14, 30141 Venezia VE, Italy) to Leone alato S.Marco (Fondamenta Andrea Navagero, 59, 30141 Venezia VE, Italy)","team":"NEUTRAL","markup":[["PLAYER",{"plain":"_\u0336\u0331\u030d_\u0334\u0333\u0349\u0306\u0308\u0301M\u0337\u0354\u0324\u0352\u0104\u0337\u030dC\u0334\u033c\u0315\u0345H\u0336\u0339\u0355\u033c\u033e\u1e2c\u0335\u0307\u033e\u0313N\u0335\u033a\u0355\u0352\u0300\u030d\u00c4\u0334\u031e\u0330\u0301_\u0334\u0326\u0300\u0346\u0313_\u0337\u0323\u0308\u0301","team":"NEUTRAL"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"A.M.G. Judo Murano (Fondamenta Antonio Colleoni, 14, 30141 Venezia VE, Italy)","name":"A.M.G. Judo Murano","address":"Fondamenta Antonio Colleoni, 14, 30141 Venezia VE, Italy","latE6":45455210,"lngE6":12354014,"team":"NEUTRAL"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Leone alato S.Marco (Fondamenta Andrea Navagero, 59, 30141 Venezia VE, Italy)","name":"Leone alato S.Marco","address":"Fondamenta Andrea Navagero, 59, 30141 Venezia VE, Italy","latE6":45454742,"lngE6":12356855,"team":"NEUTRAL"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["bc4afa30553e4323a88dba7dffdc8995.d",1719762532642,{"plext":{"text":"_\u0336\u0331\u030d_\u0334\u0333\u0349\u0306\u0308\u0301M\u0337\u0354\u0324\u0352\u0104\u0337\u030dC\u0334\u033c\u0315\u0345H\u0336\u0339\u0355\u033c\u033e\u1e2c\u0335\u0307\u033e\u0313N\u0335\u033a\u0355\u0352\u0300\u030d\u00c4\u0334\u031e\u0330\u0301_\u0334\u0326\u0300\u0346\u0313_\u0337\u0323\u0308\u0301 linked Parco Giochi Delle Piscine (Vicolo Giacomo Zanella, 67/A, 31100 Treviso TV, Italy) to Treviso - Fontanella con drago (Via Castello d'Amore, 40, 31100 Treviso TV, Italy)","team":"NEUTRAL","markup":[["PLAYER",{"plain":"_\u0336\u0331\u030d_\u0334\u0333\u0349\u0306\u0308\u0301M\u0337\u0354\u0324\u0352\u0104\u0337\u030dC\u0334\u033c\u0315\u0345H\u0336\u0339\u0355\u033c\u033e\u1e2c\u0335\u0307\u033e\u0313N\u0335\u033a\u0355\u0352\u0300\u030d\u00c4\u0334\u031e\u0330\u0301_\u0334\u0326\u0300\u0346\u0313_\u0337\u0323\u0308\u0301","team":"NEUTRAL"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Parco Giochi Delle Piscine (Vicolo Giacomo Zanella, 67/A, 31100 Treviso TV, Italy)","name":"Parco Giochi Delle Piscine","address":"Vicolo Giacomo Zanella, 67/A, 31100 Treviso TV, Italy","latE6":45670559,"lngE6":12268073,"team":"NEUTRAL"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Treviso - Fontanella con drago (Via Castello d'Amore, 40, 31100 Treviso TV, Italy)","name":"Treviso - Fontanella con drago","address":"Via Castello d'Amore, 40, 31100 Treviso TV, Italy","latE6":45673175,"lngE6":12257950,"team":"NEUTRAL"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["96692956466f413c91653f9d5c5ce1bb.d",1719762532642,{"plext":{"text":"_\u0336\u0331\u030d_\u0334\u0333\u0349\u0306\u0308\u0301M\u0337\u0354\u0324\u0352\u0104\u0337\u030dC\u0334\u033c\u0315\u0345H\u0336\u0339\u0355\u033c\u033e\u1e2c\u0335\u0307\u033e\u0313N\u0335\u033a\u0355\u0352\u0300\u030d\u00c4\u0334\u031e\u0330\u0301_\u0334\u0326\u0300\u0346\u0313_\u0337\u0323\u0308\u0301 linked Parco Giochi Delle Piscine (Vicolo Giacomo Zanella, 67/A, 31100 Treviso TV, Italy) to Selvana - Campo da Calcio (Via Giacomo Zannella, 7B, 31100 Treviso TV, Italy)","team":"NEUTRAL","markup":[["PLAYER",{"plain":"_\u0336\u0331\u030d_\u0334\u0333\u0349\u0306\u0308\u0301M\u0337\u0354\u0324\u0352\u0104\u0337\u030dC\u0334\u033c\u0315\u0345H\u0336\u0339\u0355\u033c\u033e\u1e2c\u0335\u0307\u033e\u0313N\u0335\u033a\u0355\u0352\u0300\u030d\u00c4\u0334\u031e\u0330\u0301_\u0334\u0326\u0300\u0346\u0313_\u0337\u0323\u0308\u0301","team":"NEUTRAL"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Parco Giochi Delle Piscine (Vicolo Giacomo Zanella, 67/A, 31100 Treviso TV, Italy)","name":"Parco Giochi Delle Piscine","address":"Vicolo Giacomo Zanella, 67/A, 31100 Treviso TV, Italy","latE6":45670559,"lngE6":12268073,"team":"NEUTRAL"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Selvana - Campo da Calcio (Via Giacomo Zannella, 7B, 31100 Treviso TV, Italy)","name":"Selvana - Campo da Calcio","address":"Via Giacomo Zannella, 7B, 31100 Treviso TV, Italy","latE6":45675267,"lngE6":12263910,"team":"NEUTRAL"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["80272bf3f8eb4f62af02d73e02a7d9ab.d",1719762434058,{"plext":{"text":"Resistance agent btgalpi linked from Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy) to Campo da Basket (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" linked from "}],["PORTAL",{"plain":"Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da calcio in erba sintetica","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385714,"lngE6":11799382,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Campo da Basket (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da Basket","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45386327,"lngE6":11799340,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["23b803c4335b4400a3032e7d2022596c.d",1719762434058,{"plext":{"text":"Resistance agent btgalpi created a Control Field @Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy) +1 MUs","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da calcio in erba sintetica","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385714,"lngE6":11799382,"team":"RESISTANCE"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"1"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["5631c96ab7554006a5908ba9dccd8f77.d",1719762432825,{"plext":{"text":"Resistance agent btgalpi linked from Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy) to Icona votiva in metalo (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" linked from "}],["PORTAL",{"plain":"Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da calcio in erba sintetica","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385714,"lngE6":11799382,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Icona votiva in metalo (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Icona votiva in metalo","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385771,"lngE6":11798939,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["f925d55927294293a9f9a84c942e9b8f.d",1719762409260,{"plext":{"text":"btgalpi deployed a Resonator on Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da calcio in erba sintetica","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385714,"lngE6":11799382,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["438df9c950ec4a13b527ab440f516e25.d",1719762409260,{"plext":{"text":"btgalpi captured Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da calcio in erba sintetica","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385714,"lngE6":11799382,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["acdfc2b77d2a4442b9bf037858471bc6.d",1719762372528,{"plext":{"text":"guerrafix deployed a Resonator on La tradotta. segnale con avviso accoppiato (Via Schiavonesca Vecchia, 67B, 31040 Volpago del Montello TV, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"guerrafix","team":"ENLIGHTENED"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"La tradotta. segnale con avviso accoppiato (Via Schiavonesca Vecchia, 67B, 31040 Volpago del Montello TV, Italy)","name":"La tradotta. segnale con avviso accoppiato","address":"Via Schiavonesca Vecchia, 67B, 31040 Volpago del Montello TV, Italy","latE6":45777523,"lngE6":12141673,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["3aaad33573f54a849c8c4a4814a9c2e8.d",1719762372528,{"plext":{"text":"guerrafix captured La tradotta. segnale con avviso accoppiato (Via Schiavonesca Vecchia, 67B, 31040 Volpago del Montello TV, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"guerrafix","team":"ENLIGHTENED"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"La tradotta. segnale con avviso accoppiato (Via Schiavonesca Vecchia, 67B, 31040 Volpago del Montello TV, Italy)","name":"La tradotta. segnale con avviso accoppiato","address":"Via Schiavonesca Vecchia, 67B, 31040 Volpago del Montello TV, Italy","latE6":45777523,"lngE6":12141673,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["6302ce95eb15430390357682ce3e1413.d",1719762369998,{"plext":{"text":"_\u0336\u0331\u030d_\u0334\u0333\u0349\u0306\u0308\u0301M\u0337\u0354\u0324\u0352\u0104\u0337\u030dC\u0334\u033c\u0315\u0345H\u0336\u0339\u0355\u033c\u033e\u1e2c\u0335\u0307\u033e\u0313N\u0335\u033a\u0355\u0352\u0300\u030d\u00c4\u0334\u031e\u0330\u0301_\u0334\u0326\u0300\u0346\u0313_\u0337\u0323\u0308\u0301 linked Fontana Perpetua (Via Citolo da Perugia, 1, 35138 Padova PD, Italy) to Serbatoio dell'acquedotto (Viale della Rotonda, 35138 Padua, Italy)","team":"NEUTRAL","markup":[["PLAYER",{"plain":"_\u0336\u0331\u030d_\u0334\u0333\u0349\u0306\u0308\u0301M\u0337\u0354\u0324\u0352\u0104\u0337\u030dC\u0334\u033c\u0315\u0345H\u0336\u0339\u0355\u033c\u033e\u1e2c\u0335\u0307\u033e\u0313N\u0335\u033a\u0355\u0352\u0300\u030d\u00c4\u0334\u031e\u0330\u0301_\u0334\u0326\u0300\u0346\u0313_\u0337\u0323\u0308\u0301","team":"NEUTRAL"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Fontana Perpetua (Via Citolo da Perugia, 1, 35138 Padova PD, Italy)","name":"Fontana Perpetua","address":"Via Citolo da Perugia, 1, 35138 Padova PD, Italy","latE6":45416486,"lngE6":11875661,"team":"NEUTRAL"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Serbatoio dell'acquedotto (Viale della Rotonda, 35138 Padua, Italy)","name":"Serbatoio dell'acquedotto","address":"Viale della Rotonda, 35138 Padua, Italy","latE6":45416235,"lngE6":11875290,"team":"NEUTRAL"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["8779d61a6a864989945d2feac0a9d192.d",1719762355092,{"plext":{"text":"_\u0336\u0331\u030d_\u0334\u0333\u0349\u0306\u0308\u0301M\u0337\u0354\u0324\u0352\u0104\u0337\u030dC\u0334\u033c\u0315\u0345H\u0336\u0339\u0355\u033c\u033e\u1e2c\u0335\u0307\u033e\u0313N\u0335\u033a\u0355\u0352\u0300\u030d\u00c4\u0334\u031e\u0330\u0301_\u0334\u0326\u0300\u0346\u0313_\u0337\u0323\u0308\u0301 linked Breda di Piave - Murales giochi estivi (Via Roma, 8, 31030 Breda di Piave TV, Italy) to Ufficio Postale di Breda di Piave (N,, Piazza Italia, 13, 31030 Breda di Piave TV, Italy)","team":"NEUTRAL","markup":[["PLAYER",{"plain":"_\u0336\u0331\u030d_\u0334\u0333\u0349\u0306\u0308\u0301M\u0337\u0354\u0324\u0352\u0104\u0337\u030dC\u0334\u033c\u0315\u0345H\u0336\u0339\u0355\u033c\u033e\u1e2c\u0335\u0307\u033e\u0313N\u0335\u033a\u0355\u0352\u0300\u030d\u00c4\u0334\u031e\u0330\u0301_\u0334\u0326\u0300\u0346\u0313_\u0337\u0323\u0308\u0301","team":"NEUTRAL"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Breda di Piave - Murales giochi estivi (Via Roma, 8, 31030 Breda di Piave TV, Italy)","name":"Breda di Piave - Murales giochi estivi","address":"Via Roma, 8, 31030 Breda di Piave TV, Italy","latE6":45720428,"lngE6":12329917,"team":"NEUTRAL"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Ufficio Postale di Breda di Piave (N,, Piazza Italia, 13, 31030 Breda di Piave TV, Italy)","name":"Ufficio Postale di Breda di Piave","address":"N,, Piazza Italia, 13, 31030 Breda di Piave TV, Italy","latE6":45720827,"lngE6":12332374,"team":"NEUTRAL"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["4e1a7fd446f6408399472fb0cb0a74fd.d",1719762338577,{"plext":{"text":"Resistance agent btgalpi linked from Icona votiva in metalo (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy) to Campo da Basket (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" linked from "}],["PORTAL",{"plain":"Icona votiva in metalo (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Icona votiva in metalo","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385771,"lngE6":11798939,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Campo da Basket (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da Basket","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45386327,"lngE6":11799340,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["43628d2ed1f94579969d57af8b655686.d",1719762338124,{"plext":{"text":"_\u0336\u0331\u030d_\u0334\u0333\u0349\u0306\u0308\u0301M\u0337\u0354\u0324\u0352\u0104\u0337\u030dC\u0334\u033c\u0315\u0345H\u0336\u0339\u0355\u033c\u033e\u1e2c\u0335\u0307\u033e\u0313N\u0335\u033a\u0355\u0352\u0300\u030d\u00c4\u0334\u031e\u0330\u0301_\u0334\u0326\u0300\u0346\u0313_\u0337\u0323\u0308\u0301 linked Murale Boogie - Orion (Corso Milano, 177, 35139 Padova PD, Italy) to Jump Street art (Via Digione, 3, 35138 Padova PD, Italy)","team":"NEUTRAL","markup":[["PLAYER",{"plain":"_\u0336\u0331\u030d_\u0334\u0333\u0349\u0306\u0308\u0301M\u0337\u0354\u0324\u0352\u0104\u0337\u030dC\u0334\u033c\u0315\u0345H\u0336\u0339\u0355\u033c\u033e\u1e2c\u0335\u0307\u033e\u0313N\u0335\u033a\u0355\u0352\u0300\u030d\u00c4\u0334\u031e\u0330\u0301_\u0334\u0326\u0300\u0346\u0313_\u0337\u0323\u0308\u0301","team":"NEUTRAL"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Murale Boogie - Orion (Corso Milano, 177, 35139 Padova PD, Italy)","name":"Murale Boogie - Orion","address":"Corso Milano, 177, 35139 Padova PD, Italy","latE6":45411193,"lngE6":11866572,"team":"NEUTRAL"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Jump Street art (Via Digione, 3, 35138 Padova PD, Italy)","name":"Jump Street art","address":"Via Digione, 3, 35138 Padova PD, Italy","latE6":45411968,"lngE6":11861669,"team":"NEUTRAL"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["ae5e911efa404c64a62089dbc865a959.d",1719762323258,{"plext":{"text":"btgalpi destroyed a Resonator on Bassorilievo Madonna di Fatima con crocifisso (Via S. Domenico, 212, 35030 Selvazzano Dentro PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Bassorilievo Madonna di Fatima con crocifisso (Via S. Domenico, 212, 35030 Selvazzano Dentro PD, Italy)","name":"Bassorilievo Madonna di Fatima con crocifisso","address":"Via S. Domenico, 212, 35030 Selvazzano Dentro PD, Italy","latE6":45385465,"lngE6":11799183,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["ee7a3dae4ce24c909fe9997db9439561.d",1719762321362,{"plext":{"text":"Agent btgalpi destroyed the Enlightened Link Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy) to Capitello S. Lorenzo (Via San Lorenzo, 29, 35031 Abano Terme Padua, Italy)","team":"ENLIGHTENED","markup":[["TEXT",{"plain":"Agent "}],["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed the "}],["FACTION",{"team":"ENLIGHTENED","plain":"Enlightened"}],["TEXT",{"plain":" Link "}],["PORTAL",{"plain":"Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da calcio in erba sintetica","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385714,"lngE6":11799382,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Capitello S. Lorenzo (Via San Lorenzo, 29, 35031 Abano Terme Padua, Italy)","name":"Capitello S. Lorenzo","address":"Via San Lorenzo, 29, 35031 Abano Terme Padua, Italy","latE6":45372064,"lngE6":11798754,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["d759f198aaa942a6a66a69171aa60334.d",1719762321362,{"plext":{"text":"Agent btgalpi destroyed the Enlightened Link Bassorilievo Madonna di Fatima con crocifisso (Via S. Domenico, 212, 35030 Selvazzano Dentro PD, Italy) to Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","team":"ENLIGHTENED","markup":[["TEXT",{"plain":"Agent "}],["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed the "}],["FACTION",{"team":"ENLIGHTENED","plain":"Enlightened"}],["TEXT",{"plain":" Link "}],["PORTAL",{"plain":"Bassorilievo Madonna di Fatima con crocifisso (Via S. Domenico, 212, 35030 Selvazzano Dentro PD, Italy)","name":"Bassorilievo Madonna di Fatima con crocifisso","address":"Via S. Domenico, 212, 35030 Selvazzano Dentro PD, Italy","latE6":45385465,"lngE6":11799183,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da calcio in erba sintetica","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385714,"lngE6":11799382,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["c871d0ce7a224d91bf37e75b076f0e8b.d",1719762321362,{"plext":{"text":"Agent btgalpi destroyed the Enlightened Link Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy) to Parco Comunale (Via Piemonte, 11, 35030 Selvazzano Dentro PD, Italy)","team":"ENLIGHTENED","markup":[["TEXT",{"plain":"Agent "}],["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed the "}],["FACTION",{"team":"ENLIGHTENED","plain":"Enlightened"}],["TEXT",{"plain":" Link "}],["PORTAL",{"plain":"Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da calcio in erba sintetica","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385714,"lngE6":11799382,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Parco Comunale (Via Piemonte, 11, 35030 Selvazzano Dentro PD, Italy)","name":"Parco Comunale","address":"Via Piemonte, 11, 35030 Selvazzano Dentro PD, Italy","latE6":45389606,"lngE6":11787943,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["bf9a4a7ba8ab4dbfb23f47ecae0fca5c.d",1719762321362,{"plext":{"text":"Agent btgalpi destroyed the Enlightened Control Field @Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy) -7 MUs","team":"RESISTANCE","markup":[["TEXT",{"plain":"Agent "}],["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed the "}],["FACTION",{"team":"ENLIGHTENED","plain":"Enlightened"}],["TEXT",{"plain":" Control Field @"}],["PORTAL",{"plain":"Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da calcio in erba sintetica","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385714,"lngE6":11799382,"team":"ENLIGHTENED"}],["TEXT",{"plain":" -"}],["TEXT",{"plain":"7"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["9359bf691719440daa4781e920c90f77.d",1719762321362,{"plext":{"text":"Agent btgalpi destroyed the Enlightened Control Field @Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy) -413 MUs","team":"RESISTANCE","markup":[["TEXT",{"plain":"Agent "}],["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed the "}],["FACTION",{"team":"ENLIGHTENED","plain":"Enlightened"}],["TEXT",{"plain":" Control Field @"}],["PORTAL",{"plain":"Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da calcio in erba sintetica","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385714,"lngE6":11799382,"team":"ENLIGHTENED"}],["TEXT",{"plain":" -"}],["TEXT",{"plain":"413"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["e059c927336248b8a2fb6f4704981646.d",1719762298866,{"plext":{"text":"btgalpi destroyed a Resonator on Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da calcio in erba sintetica","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385714,"lngE6":11799382,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["f2f6df3cfb8e445ba43f5498124e3483.d",1719762290341,{"plext":{"text":"guerrafix deployed a Resonator on La tradotta. 12 (Via Lavaio Basso, 50, 31040 Volpago del Montello TV, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"guerrafix","team":"ENLIGHTENED"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"La tradotta. 12 (Via Lavaio Basso, 50, 31040 Volpago del Montello TV, Italy)","name":"La tradotta. 12","address":"Via Lavaio Basso, 50, 31040 Volpago del Montello TV, Italy","latE6":45776674,"lngE6":12139036,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["2e78ba95d82c437bb217b598bf29f07a.d",1719762290341,{"plext":{"text":"guerrafix captured La tradotta. 12 (Via Lavaio Basso, 50, 31040 Volpago del Montello TV, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"guerrafix","team":"ENLIGHTENED"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"La tradotta. 12 (Via Lavaio Basso, 50, 31040 Volpago del Montello TV, Italy)","name":"La tradotta. 12","address":"Via Lavaio Basso, 50, 31040 Volpago del Montello TV, Italy","latE6":45776674,"lngE6":12139036,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["833e12d2601545aaa89aaae80273d621.d",1719762282386,{"plext":{"text":"btgalpi destroyed a Resonator on Chiesa San Domenico (Via San Giuseppe, 48, 35030 Selvazzano Dentro Province of Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Chiesa San Domenico (Via San Giuseppe, 48, 35030 Selvazzano Dentro Province of Padua, Italy)","name":"Chiesa San Domenico","address":"Via San Giuseppe, 48, 35030 Selvazzano Dentro Province of Padua, Italy","latE6":45386584,"lngE6":11798832,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["01cad042ff5449eda474a076a9761182.d",1719762277847,{"plext":{"text":"btgalpi destroyed a Resonator on Parco Perlasca (Via S. Domenico, 23, 35030 Selvazzano Dentro PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Parco Perlasca (Via S. Domenico, 23, 35030 Selvazzano Dentro PD, Italy)","name":"Parco Perlasca","address":"Via S. Domenico, 23, 35030 Selvazzano Dentro PD, Italy","latE6":45385408,"lngE6":11798846,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["ba1c258b18904ad7b6cdcc7586d9fbd0.d",1719762258921,{"plext":{"text":"Resistance agent CeccoMan linked from Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy) to Palazzo Papafava dei Carraresi (Via Cesare Battisti, 3, 35121 Padua, Italy)","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" linked from "}],["PORTAL",{"plain":"Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy)","name":"Fontana di Piazza delle Erbe","address":"Via Municipio, 35122 Padua, Province of Padua, Italy","latE6":45406794,"lngE6":11875778,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Palazzo Papafava dei Carraresi (Via Cesare Battisti, 3, 35121 Padua, Italy)","name":"Palazzo Papafava dei Carraresi","address":"Via Cesare Battisti, 3, 35121 Padua, Italy","latE6":45407108,"lngE6":11877477,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["70a651da94c74221bf7a759219febb7e.d",1719762258921,{"plext":{"text":"Resistance agent CeccoMan created a Control Field @Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy) +12 MUs","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy)","name":"Fontana di Piazza delle Erbe","address":"Via Municipio, 35122 Padua, Province of Padua, Italy","latE6":45406794,"lngE6":11875778,"team":"RESISTANCE"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"12"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["1deb07fa8b7c45beaa5b3298f09a310f.d",1719762258921,{"plext":{"text":"Resistance agent CeccoMan created a Control Field @Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy) +1 MUs","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy)","name":"Fontana di Piazza delle Erbe","address":"Via Municipio, 35122 Padua, Province of Padua, Italy","latE6":45406794,"lngE6":11875778,"team":"RESISTANCE"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"1"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["9e306bba67d94972bd188acd37873735.d",1719762257090,{"plext":{"text":"Resistance agent CeccoMan created a Control Field @Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy) +2 MUs","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy)","name":"Fontana di Piazza delle Erbe","address":"Via Municipio, 35122 Padua, Province of Padua, Italy","latE6":45406794,"lngE6":11875778,"team":"RESISTANCE"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"2"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["3e64124e0b8b4629b7cc6333fba6aa46.d",1719762257090,{"plext":{"text":"Resistance agent CeccoMan linked from Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy) to Street Art Kitty (Via Pietro D'Abano, 35139 Padua, Italy)","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" linked from "}],["PORTAL",{"plain":"Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy)","name":"Fontana di Piazza delle Erbe","address":"Via Municipio, 35122 Padua, Province of Padua, Italy","latE6":45406794,"lngE6":11875778,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Street Art Kitty (Via Pietro D'Abano, 35139 Padua, Italy)","name":"Street Art Kitty","address":"Via Pietro D'Abano, 35139 Padua, Italy","latE6":45407846,"lngE6":11875054,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["9906e832524a43a5a7094cfe78b6f86e.d",1719762203796,{"plext":{"text":"Resistance agent CeccoMan linked from Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy) to Palazzo Papafava dei Carraresi (Via Cesare Battisti, 3, 35121 Padua, Italy)","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" linked from "}],["PORTAL",{"plain":"Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy)","name":"Generale tedesco","address":"Via dei Fabbri, 18, 35122 Padua, Italy","latE6":45406674,"lngE6":11875542,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Palazzo Papafava dei Carraresi (Via Cesare Battisti, 3, 35121 Padua, Italy)","name":"Palazzo Papafava dei Carraresi","address":"Via Cesare Battisti, 3, 35121 Padua, Italy","latE6":45407108,"lngE6":11877477,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["2787164c6a374323a18f0da611c0f68d.d",1719762203796,{"plext":{"text":"Resistance agent CeccoMan created a Control Field @Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy) +15 MUs","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy)","name":"Generale tedesco","address":"Via dei Fabbri, 18, 35122 Padua, Italy","latE6":45406674,"lngE6":11875542,"team":"RESISTANCE"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"15"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["73455a2c72474aeca607fb264315f4e0.d",1719762201732,{"plext":{"text":"Resistance agent CeccoMan linked from Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy) to Street Art Kitty (Via Pietro D'Abano, 35139 Padua, Italy)","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" linked from "}],["PORTAL",{"plain":"Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy)","name":"Generale tedesco","address":"Via dei Fabbri, 18, 35122 Padua, Italy","latE6":45406674,"lngE6":11875542,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Street Art Kitty (Via Pietro D'Abano, 35139 Padua, Italy)","name":"Street Art Kitty","address":"Via Pietro D'Abano, 35139 Padua, Italy","latE6":45407846,"lngE6":11875054,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["4eb7b77d95134ab19ae6d234b69d3840.d",1719762201732,{"plext":{"text":"Resistance agent CeccoMan created a Control Field @Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy) +3 MUs","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy)","name":"Generale tedesco","address":"Via dei Fabbri, 18, 35122 Padua, Italy","latE6":45406674,"lngE6":11875542,"team":"RESISTANCE"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"3"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["564bf844b367473ca7529deaee1d6703.d",1719762199621,{"plext":{"text":"Resistance agent CeccoMan linked from Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy) to Bassorilievo con draghi (Piazza della Frutta, 35122 Padua, Italy)","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" linked from "}],["PORTAL",{"plain":"Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy)","name":"Generale tedesco","address":"Via dei Fabbri, 18, 35122 Padua, Italy","latE6":45406674,"lngE6":11875542,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Bassorilievo con draghi (Piazza della Frutta, 35122 Padua, Italy)","name":"Bassorilievo con draghi","address":"Piazza della Frutta, 35122 Padua, Italy","latE6":45407338,"lngE6":11874792,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["4e113d173c904a8e9589eaf840858c20.d",1719762196591,{"plext":{"text":"Resistance agent CeccoMan linked from Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy) to Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy)","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" linked from "}],["PORTAL",{"plain":"Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy)","name":"Generale tedesco","address":"Via dei Fabbri, 18, 35122 Padua, Italy","latE6":45406674,"lngE6":11875542,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy)","name":"Fontana di Piazza delle Erbe","address":"Via Municipio, 35122 Padua, Province of Padua, Italy","latE6":45406794,"lngE6":11875778,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["e1e6cb2464c44bf3987a31913d6c85a6.d",1719762191686,{"plext":{"text":"btgalpi deployed a Resonator on Icona votiva in metalo (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Icona votiva in metalo (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Icona votiva in metalo","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385771,"lngE6":11798939,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["2b695e2a39224b81b9d775ce62cdead4.d",1719762191686,{"plext":{"text":"btgalpi captured Icona votiva in metalo (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Icona votiva in metalo (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Icona votiva in metalo","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385771,"lngE6":11798939,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["fb5eb96964df4cad82d7428d78bb3118.d",1719762186104,{"plext":{"text":"CeccoMan deployed a Resonator on Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy)","name":"Generale tedesco","address":"Via dei Fabbri, 18, 35122 Padua, Italy","latE6":45406674,"lngE6":11875542,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["64f1abe7a6cd46389e3c57c0920ca46e.d",1719762186104,{"plext":{"text":"CeccoMan captured Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Generale tedesco (Via dei Fabbri, 18, 35122 Padua, Italy)","name":"Generale tedesco","address":"Via dei Fabbri, 18, 35122 Padua, Italy","latE6":45406674,"lngE6":11875542,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["5fa642c8c2af460a87773b281b3f398c.d",1719762182507,{"plext":{"text":"Resistance agent Z0FK4 linked from Targa gemellaggio (Via Garibaldi, 8, 33040 Pradamano UD, Italy) to La mucca (Via Lovaria, 48, 33050 Pavia di Udine UD, Italy)","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"Z0FK4","team":"RESISTANCE"}],["TEXT",{"plain":" linked from "}],["PORTAL",{"plain":"Targa gemellaggio (Via Garibaldi, 8, 33040 Pradamano UD, Italy)","name":"Targa gemellaggio","address":"Via Garibaldi, 8, 33040 Pradamano UD, Italy","latE6":46033355,"lngE6":13306374,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"La mucca (Via Lovaria, 48, 33050 Pavia di Udine UD, Italy)","name":"La mucca","address":"Via Lovaria, 48, 33050 Pavia di Udine UD, Italy","latE6":46003295,"lngE6":13303723,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["13bf9a242dc940dc874b5f41faab748c.d",1719762182507,{"plext":{"text":"Resistance agent Z0FK4 created a Control Field @Targa gemellaggio (Via Garibaldi, 8, 33040 Pradamano UD, Italy) +449 MUs","team":"RESISTANCE","markup":[["FACTION",{"team":"RESISTANCE","plain":"Resistance"}],["TEXT",{"plain":" agent "}],["PLAYER",{"plain":"Z0FK4","team":"RESISTANCE"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Targa gemellaggio (Via Garibaldi, 8, 33040 Pradamano UD, Italy)","name":"Targa gemellaggio","address":"Via Garibaldi, 8, 33040 Pradamano UD, Italy","latE6":46033355,"lngE6":13306374,"team":"RESISTANCE"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"449"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["dcaaaf2f4f43416da21ffd259f63a6c7.d",1719762128785,{"plext":{"text":"btgalpi destroyed a Resonator on Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Campo da calcio in erba sintetica (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da calcio in erba sintetica","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385714,"lngE6":11799382,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["34b025e1ae0a464d82a55eba9b03e81f.d",1719762127331,{"plext":{"text":"CeccoMan deployed a Resonator on Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy)","name":"Fontana di Piazza delle Erbe","address":"Via Municipio, 35122 Padua, Province of Padua, Italy","latE6":45406794,"lngE6":11875778,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["208316fa15274ce1bf1af61930050e3e.d",1719762127331,{"plext":{"text":"CeccoMan captured Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Fontana di Piazza delle Erbe (Via Municipio, 35122 Padua, Province of Padua, Italy)","name":"Fontana di Piazza delle Erbe","address":"Via Municipio, 35122 Padua, Province of Padua, Italy","latE6":45406794,"lngE6":11875778,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["9c4b7887849b4c579acfb0eb743c9790.d",1719762126826,{"plext":{"text":"btgalpi destroyed a Resonator on Icona votiva in metalo (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"btgalpi","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Icona votiva in metalo (Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy)","name":"Icona votiva in metalo","address":"Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy","latE6":45385771,"lngE6":11798939,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["eeac74366fe344f691bedf4bd7ca460c.d",1719762110383,{"plext":{"text":"CeccoMan destroyed a Resonator on Il Corso (Riviera Tito Livio, 12, 35123 Padua, Province of Padua, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Il Corso (Riviera Tito Livio, 12, 35123 Padua, Province of Padua, Italy)","name":"Il Corso","address":"Riviera Tito Livio, 12, 35123 Padua, Province of Padua, Italy","latE6":45405879,"lngE6":11877112,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["7c0b3cc095fa42e88b79918e33a6550e.d",1719762105468,{"plext":{"text":"Agent CeccoMan destroyed the Enlightened Link Affresco (Via San Canziano, 2, 35122 Padua, Italy) to Chiesa Di San Canziano (Via delle Piazze, 1, 35122 Padova PD, Italy)","team":"ENLIGHTENED","markup":[["TEXT",{"plain":"Agent "}],["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed the "}],["FACTION",{"team":"ENLIGHTENED","plain":"Enlightened"}],["TEXT",{"plain":" Link "}],["PORTAL",{"plain":"Affresco (Via San Canziano, 2, 35122 Padua, Italy)","name":"Affresco","address":"Via San Canziano, 2, 35122 Padua, Italy","latE6":45406483,"lngE6":11876685,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Chiesa Di San Canziano (Via delle Piazze, 1, 35122 Padova PD, Italy)","name":"Chiesa Di San Canziano","address":"Via delle Piazze, 1, 35122 Padova PD, Italy","latE6":45406307,"lngE6":11876288,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["6b8888e054014e7da32bc47fd8349ece.d",1719762105468,{"plext":{"text":"Agent CeccoMan destroyed the Neutral Link Affreschi - Leoni Marciani (Piazza Erbe, 16, 35122 Padova PD, Italy) to Colonna Papale (Piazza della Frutta, 35139 Padua, Padua, Italy)","team":"NEUTRAL","markup":[["TEXT",{"plain":"Agent "}],["PLAYER",{"plain":"CeccoMan","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed the "}],["FACTION",{"team":"NEUTRAL","plain":"Neutral"}],["TEXT",{"plain":" Link "}],["PORTAL",{"plain":"Affreschi - Leoni Marciani (Piazza Erbe, 16, 35122 Padova PD, Italy)","name":"Affreschi - Leoni Marciani","address":"Piazza Erbe, 16, 35122 Padova PD, Italy","latE6":45406791,"lngE6":11874924,"team":"NEUTRAL"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Colonna Papale (Piazza della Frutta, 35139 Padua, Padua, Italy)","name":"Colonna Papale","address":"Piazza della Frutta, 35139 Padua, Padua, Italy","latE6":45407691,"lngE6":11875351,"team":"NEUTRAL"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}]]}"#;
        use crate::entities::{Plext::*, Team::*, *};
        let expected = [
            DestroyedLink {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                source: Portal {
                    name: "Affreschi - Leoni Marciani",
                    address: "Piazza Erbe, 16, 35122 Padova PD, Italy",
                    lat: rust_decimal_macros::dec!(45.406791),
                    lon: rust_decimal_macros::dec!(11.874924),
                },
                target: Portal {
                    name: "Colonna Papale",
                    address: "Piazza della Frutta, 35139 Padua, Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.407691),
                    lon: rust_decimal_macros::dec!(11.875351),
                },
                time: 1719762105468,
            },
            DestroyedLink {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                source: Portal {
                    name: "Affresco",
                    address: "Via San Canziano, 2, 35122 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406483),
                    lon: rust_decimal_macros::dec!(11.876685),
                },
                target: Portal {
                    name: "Chiesa Di San Canziano",
                    address: "Via delle Piazze, 1, 35122 Padova PD, Italy",
                    lat: rust_decimal_macros::dec!(45.406307),
                    lon: rust_decimal_macros::dec!(11.876288),
                },
                time: 1719762105468,
            },
            DestroyedReso {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                portal: Portal {
                    name: "Il Corso",
                    address: "Riviera Tito Livio, 12, 35123 Padua, Province of Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.405879),
                    lon: rust_decimal_macros::dec!(11.877112),
                },
                time: 1719762110383,
            },
            DestroyedReso {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                portal: Portal {
                    name: "Icona votiva in metalo",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385771),
                    lon: rust_decimal_macros::dec!(11.798939),
                },
                time: 1719762126826,
            },
            Captured {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                portal: Portal {
                    name: "Fontana di Piazza delle Erbe",
                    address: "Via Municipio, 35122 Padua, Province of Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406794),
                    lon: rust_decimal_macros::dec!(11.875778),
                },
                time: 1719762127331,
            },
            DeployedReso {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                portal: Portal {
                    name: "Fontana di Piazza delle Erbe",
                    address: "Via Municipio, 35122 Padua, Province of Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406794),
                    lon: rust_decimal_macros::dec!(11.875778),
                },
                time: 1719762127331,
            },
            DestroyedReso {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                portal: Portal {
                    name: "Campo da calcio in erba sintetica",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385714),
                    lon: rust_decimal_macros::dec!(11.799382),
                },
                time: 1719762128785,
            },
            CreatedCF {
                player: Player {
                    team: Resistance,
                    name: "Z0FK4",
                },
                portal: Portal {
                    name: "Targa gemellaggio",
                    address: "Via Garibaldi, 8, 33040 Pradamano UD, Italy",
                    lat: rust_decimal_macros::dec!(46.033355),
                    lon: rust_decimal_macros::dec!(13.306374),
                },
                mu: 449,
                time: 1719762182507,
            },
            Linked {
                player: Player {
                    team: Resistance,
                    name: "Z0FK4",
                },
                source: Portal {
                    name: "Targa gemellaggio",
                    address: "Via Garibaldi, 8, 33040 Pradamano UD, Italy",
                    lat: rust_decimal_macros::dec!(46.033355),
                    lon: rust_decimal_macros::dec!(13.306374),
                },
                target: Portal {
                    name: "La mucca",
                    address: "Via Lovaria, 48, 33050 Pavia di Udine UD, Italy",
                    lat: rust_decimal_macros::dec!(46.003295),
                    lon: rust_decimal_macros::dec!(13.303723),
                },
                time: 1719762182507,
            },
            Captured {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                portal: Portal {
                    name: "Generale tedesco",
                    address: "Via dei Fabbri, 18, 35122 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406674),
                    lon: rust_decimal_macros::dec!(11.875542),
                },
                time: 1719762186104,
            },
            DeployedReso {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                portal: Portal {
                    name: "Generale tedesco",
                    address: "Via dei Fabbri, 18, 35122 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406674),
                    lon: rust_decimal_macros::dec!(11.875542),
                },
                time: 1719762186104,
            },
            Captured {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                portal: Portal {
                    name: "Icona votiva in metalo",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385771),
                    lon: rust_decimal_macros::dec!(11.798939),
                },
                time: 1719762191686,
            },
            DeployedReso {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                portal: Portal {
                    name: "Icona votiva in metalo",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385771),
                    lon: rust_decimal_macros::dec!(11.798939),
                },
                time: 1719762191686,
            },
            Linked {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                source: Portal {
                    name: "Generale tedesco",
                    address: "Via dei Fabbri, 18, 35122 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406674),
                    lon: rust_decimal_macros::dec!(11.875542),
                },
                target: Portal {
                    name: "Fontana di Piazza delle Erbe",
                    address: "Via Municipio, 35122 Padua, Province of Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406794),
                    lon: rust_decimal_macros::dec!(11.875778),
                },
                time: 1719762196591,
            },
            Linked {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                source: Portal {
                    name: "Generale tedesco",
                    address: "Via dei Fabbri, 18, 35122 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406674),
                    lon: rust_decimal_macros::dec!(11.875542),
                },
                target: Portal {
                    name: "Bassorilievo con draghi",
                    address: "Piazza della Frutta, 35122 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.407338),
                    lon: rust_decimal_macros::dec!(11.874792),
                },
                time: 1719762199621,
            },
            CreatedCF {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                portal: Portal {
                    name: "Generale tedesco",
                    address: "Via dei Fabbri, 18, 35122 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406674),
                    lon: rust_decimal_macros::dec!(11.875542),
                },
                mu: 3,
                time: 1719762201732,
            },
            Linked {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                source: Portal {
                    name: "Generale tedesco",
                    address: "Via dei Fabbri, 18, 35122 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406674),
                    lon: rust_decimal_macros::dec!(11.875542),
                },
                target: Portal {
                    name: "Street Art Kitty",
                    address: "Via Pietro D'Abano, 35139 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.407846),
                    lon: rust_decimal_macros::dec!(11.875054),
                },
                time: 1719762201732,
            },
            CreatedCF {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                portal: Portal {
                    name: "Generale tedesco",
                    address: "Via dei Fabbri, 18, 35122 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406674),
                    lon: rust_decimal_macros::dec!(11.875542),
                },
                mu: 15,
                time: 1719762203796,
            },
            Linked {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                source: Portal {
                    name: "Generale tedesco",
                    address: "Via dei Fabbri, 18, 35122 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406674),
                    lon: rust_decimal_macros::dec!(11.875542),
                },
                target: Portal {
                    name: "Palazzo Papafava dei Carraresi",
                    address: "Via Cesare Battisti, 3, 35121 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.407108),
                    lon: rust_decimal_macros::dec!(11.877477),
                },
                time: 1719762203796,
            },
            Linked {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                source: Portal {
                    name: "Fontana di Piazza delle Erbe",
                    address: "Via Municipio, 35122 Padua, Province of Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406794),
                    lon: rust_decimal_macros::dec!(11.875778),
                },
                target: Portal {
                    name: "Street Art Kitty",
                    address: "Via Pietro D'Abano, 35139 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.407846),
                    lon: rust_decimal_macros::dec!(11.875054),
                },
                time: 1719762257090,
            },
            CreatedCF {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                portal: Portal {
                    name: "Fontana di Piazza delle Erbe",
                    address: "Via Municipio, 35122 Padua, Province of Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406794),
                    lon: rust_decimal_macros::dec!(11.875778),
                },
                mu: 2,
                time: 1719762257090,
            },
            CreatedCF {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                portal: Portal {
                    name: "Fontana di Piazza delle Erbe",
                    address: "Via Municipio, 35122 Padua, Province of Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406794),
                    lon: rust_decimal_macros::dec!(11.875778),
                },
                mu: 1,
                time: 1719762258921,
            },
            CreatedCF {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                portal: Portal {
                    name: "Fontana di Piazza delle Erbe",
                    address: "Via Municipio, 35122 Padua, Province of Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406794),
                    lon: rust_decimal_macros::dec!(11.875778),
                },
                mu: 12,
                time: 1719762258921,
            },
            Linked {
                player: Player {
                    team: Resistance,
                    name: "CeccoMan",
                },
                source: Portal {
                    name: "Fontana di Piazza delle Erbe",
                    address: "Via Municipio, 35122 Padua, Province of Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.406794),
                    lon: rust_decimal_macros::dec!(11.875778),
                },
                target: Portal {
                    name: "Palazzo Papafava dei Carraresi",
                    address: "Via Cesare Battisti, 3, 35121 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.407108),
                    lon: rust_decimal_macros::dec!(11.877477),
                },
                time: 1719762258921,
            },
            DestroyedReso {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                portal: Portal {
                    name: "Parco Perlasca",
                    address: "Via S. Domenico, 23, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385408),
                    lon: rust_decimal_macros::dec!(11.798846),
                },
                time: 1719762277847,
            },
            DestroyedReso {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                portal: Portal {
                    name: "Chiesa San Domenico",
                    address: "Via San Giuseppe, 48, 35030 Selvazzano Dentro Province of Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.386584),
                    lon: rust_decimal_macros::dec!(11.798832),
                },
                time: 1719762282386,
            },
            Captured {
                player: Player {
                    team: Enlightened,
                    name: "guerrafix",
                },
                portal: Portal {
                    name: "La tradotta. 12",
                    address: "Via Lavaio Basso, 50, 31040 Volpago del Montello TV, Italy",
                    lat: rust_decimal_macros::dec!(45.776674),
                    lon: rust_decimal_macros::dec!(12.139036),
                },
                time: 1719762290341,
            },
            DeployedReso {
                player: Player {
                    team: Enlightened,
                    name: "guerrafix",
                },
                portal: Portal {
                    name: "La tradotta. 12",
                    address: "Via Lavaio Basso, 50, 31040 Volpago del Montello TV, Italy",
                    lat: rust_decimal_macros::dec!(45.776674),
                    lon: rust_decimal_macros::dec!(12.139036),
                },
                time: 1719762290341,
            },
            DestroyedReso {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                portal: Portal {
                    name: "Campo da calcio in erba sintetica",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385714),
                    lon: rust_decimal_macros::dec!(11.799382),
                },
                time: 1719762298866,
            },
            DestroyedCF {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                portal: Portal {
                    name: "Campo da calcio in erba sintetica",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385714),
                    lon: rust_decimal_macros::dec!(11.799382),
                },
                mu: 413,
                time: 1719762321362,
            },
            DestroyedCF {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                portal: Portal {
                    name: "Campo da calcio in erba sintetica",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385714),
                    lon: rust_decimal_macros::dec!(11.799382),
                },
                mu: 7,
                time: 1719762321362,
            },
            DestroyedLink {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                source: Portal {
                    name: "Campo da calcio in erba sintetica",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385714),
                    lon: rust_decimal_macros::dec!(11.799382),
                },
                target: Portal {
                    name: "Parco Comunale",
                    address: "Via Piemonte, 11, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.389606),
                    lon: rust_decimal_macros::dec!(11.787943),
                },
                time: 1719762321362,
            },
            DestroyedLink {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                source: Portal {
                    name: "Bassorilievo Madonna di Fatima con crocifisso",
                    address: "Via S. Domenico, 212, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385465),
                    lon: rust_decimal_macros::dec!(11.799183),
                },
                target: Portal {
                    name: "Campo da calcio in erba sintetica",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385714),
                    lon: rust_decimal_macros::dec!(11.799382),
                },
                time: 1719762321362,
            },
            DestroyedLink {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                source: Portal {
                    name: "Campo da calcio in erba sintetica",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385714),
                    lon: rust_decimal_macros::dec!(11.799382),
                },
                target: Portal {
                    name: "Capitello S. Lorenzo",
                    address: "Via San Lorenzo, 29, 35031 Abano Terme Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.372064),
                    lon: rust_decimal_macros::dec!(11.798754),
                },
                time: 1719762321362,
            },
            DestroyedReso {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                portal: Portal {
                    name: "Bassorilievo Madonna di Fatima con crocifisso",
                    address: "Via S. Domenico, 212, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385465),
                    lon: rust_decimal_macros::dec!(11.799183),
                },
                time: 1719762323258,
            },
            Linked {
                player: Player {
                    team: Neutral,
                    name: "_\u{336}\u{331}\u{30d}_\u{334}\u{333}\u{349}\u{306}\u{308}\u{301}M\u{337}\u{354}\u{324}\u{352}Ą\u{337}\u{30d}C\u{334}\u{33c}\u{315}\u{345}H\u{336}\u{339}\u{355}\u{33c}\u{33e}Ḭ\u{335}\u{307}\u{33e}\u{313}N\u{335}\u{33a}\u{355}\u{352}\u{300}\u{30d}Ä\u{334}\u{31e}\u{330}\u{301}_\u{334}\u{326}\u{300}\u{346}\u{313}_\u{337}\u{323}\u{308}\u{301}",
                },
                source: Portal {
                    name: "Murale Boogie - Orion",
                    address: "Corso Milano, 177, 35139 Padova PD, Italy",
                    lat: rust_decimal_macros::dec!(45.411193),
                    lon: rust_decimal_macros::dec!(11.866572),
                },
                target: Portal {
                    name: "Jump Street art",
                    address: "Via Digione, 3, 35138 Padova PD, Italy",
                    lat: rust_decimal_macros::dec!(45.411968),
                    lon: rust_decimal_macros::dec!(11.861669),
                },
                time: 1719762338124,
            },
            Linked {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                source: Portal {
                    name: "Icona votiva in metalo",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385771),
                    lon: rust_decimal_macros::dec!(11.798939),
                },
                target: Portal {
                    name: "Campo da Basket",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.386327),
                    lon: rust_decimal_macros::dec!(11.799340),
                },
                time: 1719762338577,
            },
            Linked {
                player: Player {
                    team: Neutral,
                    name: "_\u{336}\u{331}\u{30d}_\u{334}\u{333}\u{349}\u{306}\u{308}\u{301}M\u{337}\u{354}\u{324}\u{352}Ą\u{337}\u{30d}C\u{334}\u{33c}\u{315}\u{345}H\u{336}\u{339}\u{355}\u{33c}\u{33e}Ḭ\u{335}\u{307}\u{33e}\u{313}N\u{335}\u{33a}\u{355}\u{352}\u{300}\u{30d}Ä\u{334}\u{31e}\u{330}\u{301}_\u{334}\u{326}\u{300}\u{346}\u{313}_\u{337}\u{323}\u{308}\u{301}",
                },
                source: Portal {
                    name: "Breda di Piave - Murales giochi estivi",
                    address: "Via Roma, 8, 31030 Breda di Piave TV, Italy",
                    lat: rust_decimal_macros::dec!(45.720428),
                    lon: rust_decimal_macros::dec!(12.329917),
                },
                target: Portal {
                    name: "Ufficio Postale di Breda di Piave",
                    address: "N,, Piazza Italia, 13, 31030 Breda di Piave TV, Italy",
                    lat: rust_decimal_macros::dec!(45.720827),
                    lon: rust_decimal_macros::dec!(12.332374),
                },
                time: 1719762355092,
            },
            Linked {
                player: Player {
                    team: Neutral,
                    name: "_\u{336}\u{331}\u{30d}_\u{334}\u{333}\u{349}\u{306}\u{308}\u{301}M\u{337}\u{354}\u{324}\u{352}Ą\u{337}\u{30d}C\u{334}\u{33c}\u{315}\u{345}H\u{336}\u{339}\u{355}\u{33c}\u{33e}Ḭ\u{335}\u{307}\u{33e}\u{313}N\u{335}\u{33a}\u{355}\u{352}\u{300}\u{30d}Ä\u{334}\u{31e}\u{330}\u{301}_\u{334}\u{326}\u{300}\u{346}\u{313}_\u{337}\u{323}\u{308}\u{301}",
                },
                source: Portal {
                    name: "Fontana Perpetua",
                    address: "Via Citolo da Perugia, 1, 35138 Padova PD, Italy",
                    lat: rust_decimal_macros::dec!(45.416486),
                    lon: rust_decimal_macros::dec!(11.875661),
                },
                target: Portal {
                    name: "Serbatoio dell'acquedotto",
                    address: "Viale della Rotonda, 35138 Padua, Italy",
                    lat: rust_decimal_macros::dec!(45.416235),
                    lon: rust_decimal_macros::dec!(11.875290),
                },
                time: 1719762369998,
            },
            Captured {
                player: Player {
                    team: Enlightened,
                    name: "guerrafix",
                },
                portal: Portal {
                    name: "La tradotta. segnale con avviso accoppiato",
                    address: "Via Schiavonesca Vecchia, 67B, 31040 Volpago del Montello TV, Italy",
                    lat: rust_decimal_macros::dec!(45.777523),
                    lon: rust_decimal_macros::dec!(12.141673),
                },
                time: 1719762372528,
            },
            DeployedReso {
                player: Player {
                    team: Enlightened,
                    name: "guerrafix",
                },
                portal: Portal {
                    name: "La tradotta. segnale con avviso accoppiato",
                    address: "Via Schiavonesca Vecchia, 67B, 31040 Volpago del Montello TV, Italy",
                    lat: rust_decimal_macros::dec!(45.777523),
                    lon: rust_decimal_macros::dec!(12.141673),
                },
                time: 1719762372528,
            },
            Captured {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                portal: Portal {
                    name: "Campo da calcio in erba sintetica",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385714),
                    lon: rust_decimal_macros::dec!(11.799382),
                },
                time: 1719762409260,
            },
            DeployedReso {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                portal: Portal {
                    name: "Campo da calcio in erba sintetica",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385714),
                    lon: rust_decimal_macros::dec!(11.799382),
                },
                time: 1719762409260,
            },
            Linked {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                source: Portal {
                    name: "Campo da calcio in erba sintetica",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385714),
                    lon: rust_decimal_macros::dec!(11.799382),
                },
                target: Portal {
                    name: "Icona votiva in metalo",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385771),
                    lon: rust_decimal_macros::dec!(11.798939),
                },
                time: 1719762432825,
            },
            CreatedCF {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                portal: Portal {
                    name: "Campo da calcio in erba sintetica",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385714),
                    lon: rust_decimal_macros::dec!(11.799382),
                },
                mu: 1,
                time: 1719762434058,
            },
            Linked {
                player: Player {
                    team: Resistance,
                    name: "btgalpi",
                },
                source: Portal {
                    name: "Campo da calcio in erba sintetica",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.385714),
                    lon: rust_decimal_macros::dec!(11.799382),
                },
                target: Portal {
                    name: "Campo da Basket",
                    address: "Via S. Domenico, 12, 35030 Selvazzano Dentro PD, Italy",
                    lat: rust_decimal_macros::dec!(45.386327),
                    lon: rust_decimal_macros::dec!(11.799340),
                },
                time: 1719762434058,
            },
            Linked {
                player: Player {
                    team: Neutral,
                    name: "_\u{336}\u{331}\u{30d}_\u{334}\u{333}\u{349}\u{306}\u{308}\u{301}M\u{337}\u{354}\u{324}\u{352}Ą\u{337}\u{30d}C\u{334}\u{33c}\u{315}\u{345}H\u{336}\u{339}\u{355}\u{33c}\u{33e}Ḭ\u{335}\u{307}\u{33e}\u{313}N\u{335}\u{33a}\u{355}\u{352}\u{300}\u{30d}Ä\u{334}\u{31e}\u{330}\u{301}_\u{334}\u{326}\u{300}\u{346}\u{313}_\u{337}\u{323}\u{308}\u{301}",
                },
                source: Portal {
                    name: "Parco Giochi Delle Piscine",
                    address: "Vicolo Giacomo Zanella, 67/A, 31100 Treviso TV, Italy",
                    lat: rust_decimal_macros::dec!(45.670559),
                    lon: rust_decimal_macros::dec!(12.268073),
                },
                target: Portal {
                    name: "Selvana - Campo da Calcio",
                    address: "Via Giacomo Zannella, 7B, 31100 Treviso TV, Italy",
                    lat: rust_decimal_macros::dec!(45.675267),
                    lon: rust_decimal_macros::dec!(12.263910),
                },
                time: 1719762532642,
            },
            Linked {
                player: Player {
                    team: Neutral,
                    name: "_\u{336}\u{331}\u{30d}_\u{334}\u{333}\u{349}\u{306}\u{308}\u{301}M\u{337}\u{354}\u{324}\u{352}Ą\u{337}\u{30d}C\u{334}\u{33c}\u{315}\u{345}H\u{336}\u{339}\u{355}\u{33c}\u{33e}Ḭ\u{335}\u{307}\u{33e}\u{313}N\u{335}\u{33a}\u{355}\u{352}\u{300}\u{30d}Ä\u{334}\u{31e}\u{330}\u{301}_\u{334}\u{326}\u{300}\u{346}\u{313}_\u{337}\u{323}\u{308}\u{301}",
                },
                source: Portal {
                    name: "Parco Giochi Delle Piscine",
                    address: "Vicolo Giacomo Zanella, 67/A, 31100 Treviso TV, Italy",
                    lat: rust_decimal_macros::dec!(45.670559),
                    lon: rust_decimal_macros::dec!(12.268073),
                },
                target: Portal {
                    name: "Treviso - Fontanella con drago",
                    address: "Via Castello d'Amore, 40, 31100 Treviso TV, Italy",
                    lat: rust_decimal_macros::dec!(45.673175),
                    lon: rust_decimal_macros::dec!(12.257950),
                },
                time: 1719762532642,
            },
            Linked {
                player: Player {
                    team: Neutral,
                    name: "_\u{336}\u{331}\u{30d}_\u{334}\u{333}\u{349}\u{306}\u{308}\u{301}M\u{337}\u{354}\u{324}\u{352}Ą\u{337}\u{30d}C\u{334}\u{33c}\u{315}\u{345}H\u{336}\u{339}\u{355}\u{33c}\u{33e}Ḭ\u{335}\u{307}\u{33e}\u{313}N\u{335}\u{33a}\u{355}\u{352}\u{300}\u{30d}Ä\u{334}\u{31e}\u{330}\u{301}_\u{334}\u{326}\u{300}\u{346}\u{313}_\u{337}\u{323}\u{308}\u{301}",
                },
                source: Portal {
                    name: "A.M.G. Judo Murano",
                    address: "Fondamenta Antonio Colleoni, 14, 30141 Venezia VE, Italy",
                    lat: rust_decimal_macros::dec!(45.455210),
                    lon: rust_decimal_macros::dec!(12.354014),
                },
                target: Portal {
                    name: "Leone alato S.Marco",
                    address: "Fondamenta Andrea Navagero, 59, 30141 Venezia VE, Italy",
                    lat: rust_decimal_macros::dec!(45.454742),
                    lon: rust_decimal_macros::dec!(12.356855),
                },
                time: 1719762782388,
            },
            Linked {
                player: Player {
                    team: Neutral,
                    name: "_\u{336}\u{331}\u{30d}_\u{334}\u{333}\u{349}\u{306}\u{308}\u{301}M\u{337}\u{354}\u{324}\u{352}Ą\u{337}\u{30d}C\u{334}\u{33c}\u{315}\u{345}H\u{336}\u{339}\u{355}\u{33c}\u{33e}Ḭ\u{335}\u{307}\u{33e}\u{313}N\u{335}\u{33a}\u{355}\u{352}\u{300}\u{30d}Ä\u{334}\u{31e}\u{330}\u{301}_\u{334}\u{326}\u{300}\u{346}\u{313}_\u{337}\u{323}\u{308}\u{301}",
                },
                source: Portal {
                    name: "A.M.G. Judo Murano",
                    address: "Fondamenta Antonio Colleoni, 14, 30141 Venezia VE, Italy",
                    lat: rust_decimal_macros::dec!(45.455210),
                    lon: rust_decimal_macros::dec!(12.354014),
                },
                target: Portal {
                    name: "Poste Italiane Murano LY",
                    address: "Fondamenta Antonio Maschio, 47-48, 30141 Venezia VE, Italy",
                    lat: rust_decimal_macros::dec!(45.455478),
                    lon: rust_decimal_macros::dec!(12.356645),
                },
                time: 1719762782388,
            },
        ];

        let res: ingress_intel_rs::plexts::IntelResponse = serde_json::from_str(s).unwrap();

        res.result
            .iter()
            .rev()
            .map(|(_id, time, plext)| {
                let msg_type = crate::entities::PlextType::from(plext.plext.markup.as_slice());
                crate::entities::Plext::try_from((msg_type, &plext.plext, *time)).unwrap()
            })
            .zip(expected)
            .for_each(|(result, expected)| assert_eq!(result, expected));
    }
}
