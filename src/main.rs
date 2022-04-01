use std::{collections::HashMap, env, future, time::Duration};

use chrono::Utc;

use futures_util::{stream::unfold, Stream, StreamExt};

use ingress_intel_rs::{
    plexts::Tab,
    portal_details::{IntelMod, IntelPortal, IntelResonator},
    Intel,
};

use lru_time_cache::LruCache;

use once_cell::sync::Lazy;

use serde_json::json;

use stream_throttle::{ThrottlePool, ThrottleRate, ThrottledStream};

use tokio::{sync::mpsc, time};

use tracing::{debug, error, info, warn};

mod config;
mod dedup_flatten;
mod entities;

type Senders = HashMap<u64, mpsc::UnboundedSender<String>>;

static USERNAME: Lazy<Option<String>> = Lazy::new(|| env::var("USERNAME").ok());
static PASSWORD: Lazy<Option<String>> = Lazy::new(|| env::var("PASSWORD").ok());
static COOKIES: Lazy<Option<String>> = Lazy::new(|| env::var("COOKIES").ok());
static BOT_TOKEN: Lazy<String> =
    Lazy::new(|| env::var("BOT_TOKEN").expect("Missing env var BOT_TOKEN"));
static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .unwrap()
});

fn make_stream<T>(rx: mpsc::UnboundedReceiver<T>) -> impl Stream<Item = T> {
    unfold(rx, |mut rx| async {
        let next = rx.recv().await?;
        Some((next, rx))
    })
}

#[tokio::main]
async fn main() {
    println!("Started version {}", env!("CARGO_PKG_VERSION"));
    tracing_subscriber::fmt::init();
    let url = format!(
        "https://api.telegram.org/bot{}/sendMessage",
        BOT_TOKEN.as_str()
    );
    let config = config::get().await.unwrap();

    let (global_tx, global_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        // we can send globally only 30 telegram messages per second
        let rate = ThrottleRate::new(30, Duration::from_secs(1));
        let pool = ThrottlePool::new(rate);
        let u = &url;
        make_stream(global_rx)
            .throttle(pool)
            .for_each_concurrent(None, |(user_id, msg): (u64, String)| async move {
                for i in 0..10 {
                    let res = CLIENT
                        .post(u)
                        .header("Content-Type", "application/json")
                        .json(&json!({
                            "chat_id": user_id,
                            "text": msg.as_str(),
                            "parse_mode": "HTML",
                            "disable_web_page_preview": true
                        }))
                        .send()
                        .await
                        .map_err(|e| {
                            error!(
                                "Telegram error on retry {}: {}\nuser_id: {}\nmessage: {}",
                                i, e, user_id, msg
                            )
                        });
                    if let Ok(res) = res {
                        if res.status().is_success() {
                            break;
                        }

                        error!(
                            "Telegram error on retry {}: {:?}\nuser_id: {}\nmessage: {}",
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
        .map(|z| {
            z.users.iter().map(|(id, _)| {
                let (tx, rx) = mpsc::unbounded_channel();
                let global_tx = global_tx.clone();
                let id = *id;
                tokio::spawn(async move {
                    // We can send a single message per telegram chat per second
                    let rate = ThrottleRate::new(1, Duration::from_secs(1));
                    let pool = ThrottlePool::new(rate);
                    make_stream(rx)
                        .throttle(pool)
                        .for_each(|msg| {
                            global_tx
                                .send((id, msg))
                                .map_err(|e| error!("Sender error: {}", e))
                                .ok();
                            future::ready(())
                        })
                        .await;
                });
                (id, tx)
            })
        })
        .flatten()
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

    intel.login().await.unwrap();

    let config = &config;
    let intel = &intel;
    let senders = &senders;
    tokio::select! {
        _ = comm_survey(&config, &intel, &senders) => {},
        _ = portal_survey(&config, &intel, &senders) => {},
    };
}

async fn comm_survey(config: &config::Config, intel: &Intel<'static>, senders: &Senders) {
    let mut interval = time::interval(Duration::from_secs(30));// main comm interval
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
                            entities::Plext::try_from((msg_type, &plext.plext, *time))
                                .map_err(|_| {
                                    error!("Unable to create {:?} from {:?}", msg_type, plext.plext)
                                })
                                .ok()
                        })
                        .collect::<Vec<_>>();
                    let msgs = dedup_flatten::windows_dedup_flatten(plexts.clone(), 8);
                    if res.result.len() != msgs.len() {
                        info!("Processed {} plexts: {:?}\nInto {} raw messages: {:?}\nInto {} messages: {:?}", res.result.len(), res.result, plexts.len(), plexts, msgs.len(), msgs);
                    }

                    for msg in msgs.iter().filter(|m| !m.has_duplicates(&msgs)) {
                        if sent_cache[index]
                            .notify_insert(msg.to_string(), ())
                            .0
                            .is_none()
                        {
                            for (id, filter) in &zone.users {
                                if filter.apply(msg) {
                                    senders[id]
                                        .send(msg.to_string())
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
                },
                Err(ingress_intel_rs::Error::Deserialize) => {
                    warn!("Probably rate limited");
                    return;
                },
                _ => {},
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
            name: p.get_name().to_owned(),
            coords: (p.get_latitude(), p.get_longitude()),
            mods: p.get_mods().to_vec(),
            resonators: p.get_resonators().to_vec(),
        }
    }
}

impl PortalCache {
    fn alarm(&self, other: &Self) -> Option<String> {
        let old = self.get_mods_count();
        let new = other.get_mods_count();
        if old > new {
            return Some(format!("{}Portal <a href=\"https://intel.ingress.com/intel?pll={},{}\">{}</a> lost {} mods since last check ({} remaining)", Self::get_symbol(), self.coords.0, self.coords.1, self.name, old - new, new));
        }

        let old = self.get_resonators_count();
        let new = other.get_resonators_count();
        if old > new {
            return Some(format!("{}Portal <a href=\"https://intel.ingress.com/intel?pll={},{}\">{}</a> lost {} resonators since last check ({} remaining)", Self::get_symbol(), self.coords.0, self.coords.1, self.name, old - new, new));
        }

        let old = self.get_resonators_energy_sum();
        let new = other.get_resonators_energy_sum();
        if old > new {
            let max = self.get_resonators_max_energy_sum();
            let lost_perc = calc_perc(old - new, max);
            // if lost_perc != 15 || !self.all_resonators_lost_the_same(other, lost_perc) {
            let left_perc = calc_perc(new, max);
            return Some(format!("{}Portal <a href=\"https://intel.ingress.com/intel?pll={},{}\">{}</a> lost {}% of resonators energy since last check ({}% remaining)", Self::get_symbol(), self.coords.0, self.coords.1, self.name, lost_perc, left_perc));
            // }
        }

        None
    }

    fn get_symbol() -> String {
        unsafe { String::from_utf8_unchecked(vec![0xE2, 0x9A, 0xA0]) }
    }

    fn get_mods_count(&self) -> usize {
        self.mods.iter().filter(|m| m.is_some()).count()
    }

    fn get_resonators_count(&self) -> usize {
        self.resonators.iter().filter(|r| r.is_some()).count()
    }

    fn get_resonators_energy_sum(&self) -> u16 {
        self.resonators
            .iter()
            .filter_map(|r| r.as_ref().map(|r| r.get_energy()))
            .sum()
    }

    fn get_resonators_max_energy_sum(&self) -> u16 {
        self.resonators
            .iter()
            .filter_map(|r| {
                r.as_ref()
                    .map(|r| get_portal_max_energy_by_level(r.get_level()))
            })
            .sum()
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
    for (_, zone) in config.zones.iter().enumerate() {
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
    let mut interval = time::interval(Duration::from_secs(10));// main portal interval
    loop {
        for (portal_id, users) in &portals {
            interval.tick().await;
            match intel.get_portal_details(portal_id).await {
                Ok(res) => {
                    let new_cache = PortalCache::from(res.result);
                    if let Some(cached) = cache.get(portal_id) {
                        if let Some(msg) = cached.alarm(&new_cache) {
                            for user_id in users {
                                senders[user_id]
                                    .send(msg.clone())
                                    .map_err(|e| error!("Sender error: {}", e))
                                    .ok();
                            }
                        }
                    }
                    cache.insert(*portal_id, new_cache);
                },
                Err(ingress_intel_rs::Error::Deserialize) => {
                    warn!("Probably rate limited, restart");
                    return;
                },
                _ => {},
            }
        }
    }
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
                    crate::entities::Plext::try_from((msg_type, &plext.plext, *time)).ok()
                })
                .collect::<Vec<_>>();
            let msgs = crate::dedup_flatten::windows_dedup_flatten(plexts, 8);
            for (index, zone) in config.zones.iter().enumerate() {
                for msg in msgs.iter().filter(|m| !m.has_duplicates(&msgs)) {
                    for (id, filter) in &zone.users {
                        if filter.apply(msg) {
                            info!("Sent message to user {} in zone {}: {:?}", id, index, msg);
                        } else {
                            debug!(
                                "Filtered message for user {} in zone {}: {:?}",
                                id, index, msg
                            );
                        }
                    }
                }
            }
        }
    }
}
