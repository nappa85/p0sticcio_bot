use std::{collections::HashMap, env, future, time::Duration};

use chrono::Utc;

use futures_util::{stream::unfold, Stream, StreamExt};

use ingress_intel_rs::{plexts::Tab, Intel};

use lru_time_cache::LruCache;

use once_cell::sync::Lazy;

use serde_json::json;

use stream_throttle::{ThrottlePool, ThrottleRate, ThrottledStream};

use tokio::{sync::mpsc, time};

use tracing::{debug, error, info};

mod config;
mod dedup_flatten;
mod entities;

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

    let mut intel = Intel::new(&CLIENT, USERNAME.as_deref(), PASSWORD.as_deref());

    if let Some(cookies) = &*COOKIES {
        for cookie in cookies.split("; ") {
            if let Some((pos, _)) = cookie.match_indices('=').next() {
                intel.add_cookie(&cookie[0..pos], &cookie[(pos + 1)..]);
            }
        }
    }

    let mut interval = time::interval(Duration::from_secs(30));
    let mut sent_cache: Vec<LruCache<String, ()>> =
        vec![LruCache::with_expiry_duration(Duration::from_secs(120)); config.zones.len()]; //2 minutes cache
    loop {
        for (index, zone) in config.zones.iter().enumerate() {
            interval.tick().await;
            if let Ok(res) = intel
                .get_plexts(
                    zone.from,
                    zone.to,
                    Tab::All,
                    Some((Utc::now() - chrono::Duration::seconds(120)).timestamp_millis()),
                    None,
                )
                .await
            {
                debug!("Got {} plexts", res.result.len());
                if res.result.is_empty() {
                    continue;
                }

                let plexts = res
                    .result
                    .iter()
                    .rev()
                    .filter_map(|(_id, _time, plext)| {
                        let msg_type = entities::PlextType::from(plext.plext.markup.as_slice());
                        entities::Plext::try_from((msg_type, &plext.plext))
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
            r#"{"result":[["15a7eb2434dd428a829ae558dc82b85d.d",1638033841061,{"plext":{"text":"TerminateThat destroyed the Link Villanova - Centro Culturale Tommasoni (Via Mussolini, 13, 35010 Mussolini PD, Italy) to Madonna Ausiliatrice (Via G. Prati, 44, 30038 Fornase VE, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"TerminateThat","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed the Link "}],["PORTAL",{"plain":"Villanova - Centro Culturale Tommasoni (Via Mussolini, 13, 35010 Mussolini PD, Italy)","name":"Villanova - Centro Culturale Tommasoni","address":"Via Mussolini, 13, 35010 Mussolini PD, Italy","latE6":45510108,"lngE6":11979066,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Madonna Ausiliatrice (Via G. Prati, 44, 30038 Fornase VE, Italy)","name":"Madonna Ausiliatrice","address":"Via G. Prati, 44, 30038 Fornase VE, Italy","latE6":45473113,"lngE6":12159412,"team":"NEUTRAL"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["15a7eb2434dd428a829ae558dc82b85d.d",1638033841061,{"plext":{"text":"TerminateThat destroyed the Link Villanova - Centro Culturale Tommasoni (Via Mussolini, 13, 35010 Mussolini PD, Italy) to Madonna Ausiliatrice (Via G. Prati, 44, 30038 Fornase VE, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"TerminateThat","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed the Link "}],["PORTAL",{"plain":"Villanova - Centro Culturale Tommasoni (Via Mussolini, 13, 35010 Mussolini PD, Italy)","name":"Villanova - Centro Culturale Tommasoni","address":"Via Mussolini, 13, 35010 Mussolini PD, Italy","latE6":45510108,"lngE6":11979066,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Madonna Ausiliatrice (Via G. Prati, 44, 30038 Fornase VE, Italy)","name":"Madonna Ausiliatrice","address":"Via G. Prati, 44, 30038 Fornase VE, Italy","latE6":45473113,"lngE6":12159412,"team":"NEUTRAL"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["d5b5a38017b14f9c90089532d4f3229b.d",1638033840743,{"plext":{"text":"Dreifel deployed a Resonator on S. LUCIA (Stazione di Venezia Santa Lucia, 30121 Venezia VE, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Dreifel","team":"ENLIGHTENED"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"S. LUCIA (Stazione di Venezia Santa Lucia, 30121 Venezia VE, Italy)","name":"S. LUCIA","address":"Stazione di Venezia Santa Lucia, 30121 Venezia VE, Italy","latE6":45440701,"lngE6":12320542,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["c9a9f9c8675e46509bb4e8e29771515b.d",1638033840743,{"plext":{"text":"Dreifel captured S. LUCIA (Stazione di Venezia Santa Lucia, 30121 Venezia VE, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Dreifel","team":"ENLIGHTENED"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"S. LUCIA (Stazione di Venezia Santa Lucia, 30121 Venezia VE, Italy)","name":"S. LUCIA","address":"Stazione di Venezia Santa Lucia, 30121 Venezia VE, Italy","latE6":45440701,"lngE6":12320542,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["19b02e7c6ffd4f54b14a7260f80c98dd.d",1638033828531,{"plext":{"text":"TerminateThat destroyed a Resonator on Madonna Ausiliatrice (Via G. Prati, 44, 30038 Fornase VE, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"TerminateThat","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Madonna Ausiliatrice (Via G. Prati, 44, 30038 Fornase VE, Italy)","name":"Madonna Ausiliatrice","address":"Via G. Prati, 44, 30038 Fornase VE, Italy","latE6":45473113,"lngE6":12159412,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["8846d91bcc2741e4940e94f40ff8e227.d",1638033783380,{"plext":{"text":"Smith82v2 linked Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy) to Ponte Ciclopedonale Cipressina (Via del Gazzato, 2a, 30174 Venezia VE, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy)","name":"Mestre - Agli Eroi di Nassirya","address":"Via Amerigo Vespucci, 30173 Venezia VE, Italy","latE6":45497392,"lngE6":12249866,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Ponte Ciclopedonale Cipressina (Via del Gazzato, 2a, 30174 Venezia VE, Italy)","name":"Ponte Ciclopedonale Cipressina","address":"Via del Gazzato, 2a, 30174 Venezia VE, Italy","latE6":45500670,"lngE6":12230719,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["80baddfa881f49f5a117035bde8555ec.d",1638033783380,{"plext":{"text":"Smith82v2 created a Control Field @Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy) +47 MUs","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy)","name":"Mestre - Agli Eroi di Nassirya","address":"Via Amerigo Vespucci, 30173 Venezia VE, Italy","latE6":45497392,"lngE6":12249866,"team":"ENLIGHTENED"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"47"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["43faa6d9ad164b388af7c9fb1213e158.d",1638033783380,{"plext":{"text":"Smith82v2 created a Control Field @Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy) +484 MUs","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy)","name":"Mestre - Agli Eroi di Nassirya","address":"Via Amerigo Vespucci, 30173 Venezia VE, Italy","latE6":45497392,"lngE6":12249866,"team":"ENLIGHTENED"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"484"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["3b9ef90b9e874bcaa4f2053fd2607306.d",1638033775607,{"plext":{"text":"Smith82v2 linked Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy) to Capitello via Portara (Via Gino Rocca, 7, 30174 Venezia VE, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy)","name":"Mestre - Agli Eroi di Nassirya","address":"Via Amerigo Vespucci, 30173 Venezia VE, Italy","latE6":45497392,"lngE6":12249866,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Capitello via Portara (Via Gino Rocca, 7, 30174 Venezia VE, Italy)","name":"Capitello via Portara","address":"Via Gino Rocca, 7, 30174 Venezia VE, Italy","latE6":45507935,"lngE6":12250628,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["da9c974c03b942328c3bdd8453211c07.d",1638033763430,{"plext":{"text":"Smith82v2 created a Control Field @Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy) +35 MUs","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy)","name":"Mestre - Agli Eroi di Nassirya","address":"Via Amerigo Vespucci, 30173 Venezia VE, Italy","latE6":45497392,"lngE6":12249866,"team":"ENLIGHTENED"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"35"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["6f5b5c29028640cdad80f11a65ab0c6e.d",1638033763430,{"plext":{"text":"Smith82v2 linked Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy) to Mestre - Targa Ai Caduti D'Europa (Via Torre Belfredo, 124, 30174 Mestre, Venice, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy)","name":"Mestre - Agli Eroi di Nassirya","address":"Via Amerigo Vespucci, 30173 Venezia VE, Italy","latE6":45497392,"lngE6":12249866,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Mestre - Targa Ai Caduti D'Europa (Via Torre Belfredo, 124, 30174 Mestre, Venice, Italy)","name":"Mestre - Targa Ai Caduti D'Europa","address":"Via Torre Belfredo, 124, 30174 Mestre, Venice, Italy","latE6":45497660,"lngE6":12239969,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["fb42b32c263740d89c77b8d87908c07a.d",1638033753532,{"plext":{"text":"Smith82v2 linked Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy) to Colazione Da Tiffany (Via Bissuola, 21, 30173 Venezia, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy)","name":"Mestre - Agli Eroi di Nassirya","address":"Via Amerigo Vespucci, 30173 Venezia VE, Italy","latE6":45497392,"lngE6":12249866,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Colazione Da Tiffany (Via Bissuola, 21, 30173 Venezia, Italy)","name":"Colazione Da Tiffany","address":"Via Bissuola, 21, 30173 Venezia, Italy","latE6":45495314,"lngE6":12250504,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["90cde2ecd7fe4f8db74f6869a455aa34.d",1638033753532,{"plext":{"text":"Smith82v2 created a Control Field @Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy) +22 MUs","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy)","name":"Mestre - Agli Eroi di Nassirya","address":"Via Amerigo Vespucci, 30173 Venezia VE, Italy","latE6":45497392,"lngE6":12249866,"team":"ENLIGHTENED"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"22"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["b6f081c2b5fd4f99b9f3ffce8d3cd3b1.d",1638033743344,{"plext":{"text":"Tomdoc deployed a Resonator on Campo Santa Maria Formosa (Fondamenta Preti Castello, 30122 Venice, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Tomdoc","team":"ENLIGHTENED"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Campo Santa Maria Formosa (Fondamenta Preti Castello, 30122 Venice, Italy)","name":"Campo Santa Maria Formosa","address":"Fondamenta Preti Castello, 30122 Venice, Italy","latE6":45437188,"lngE6":12340927,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["237562f103694987b7fd9cc900cc4270.d",1638033743344,{"plext":{"text":"Tomdoc captured Campo Santa Maria Formosa (Fondamenta Preti Castello, 30122 Venice, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Tomdoc","team":"ENLIGHTENED"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Campo Santa Maria Formosa (Fondamenta Preti Castello, 30122 Venice, Italy)","name":"Campo Santa Maria Formosa","address":"Fondamenta Preti Castello, 30122 Venice, Italy","latE6":45437188,"lngE6":12340927,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["dd6b4a6032c14ad4bd8ae23f4f498110.d",1638033728890,{"plext":{"text":"Tomdoc deployed a Resonator on E La Madonna (Ruga Giuffa, 6131, 30122 Venezia, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Tomdoc","team":"ENLIGHTENED"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"E La Madonna (Ruga Giuffa, 6131, 30122 Venezia, Italy)","name":"E La Madonna","address":"Ruga Giuffa, 6131, 30122 Venezia, Italy","latE6":45437408,"lngE6":12341267,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["94f07b0df06a47f0a7ddc03379ed6aaa.d",1638033728890,{"plext":{"text":"Tomdoc captured E La Madonna (Ruga Giuffa, 6131, 30122 Venezia, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Tomdoc","team":"ENLIGHTENED"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"E La Madonna (Ruga Giuffa, 6131, 30122 Venezia, Italy)","name":"E La Madonna","address":"Ruga Giuffa, 6131, 30122 Venezia, Italy","latE6":45437408,"lngE6":12341267,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["c31d71c4d73a4309baddf145c5e8eb92.d",1638033711108,{"plext":{"text":"Tomdoc captured G. Battista Menier (Calle Seconda de la Fava, 5263, 30122 Venezia VE, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Tomdoc","team":"ENLIGHTENED"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"G. Battista Menier (Calle Seconda de la Fava, 5263, 30122 Venezia VE, Italy)","name":"G. Battista Menier","address":"Calle Seconda de la Fava, 5263, 30122 Venezia VE, Italy","latE6":45436898,"lngE6":12341097,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["0a7004e1be7240dbae6e5399499f38b8.d",1638033711108,{"plext":{"text":"Tomdoc deployed a Resonator on G. Battista Menier (Calle Seconda de la Fava, 5263, 30122 Venezia VE, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Tomdoc","team":"ENLIGHTENED"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"G. Battista Menier (Calle Seconda de la Fava, 5263, 30122 Venezia VE, Italy)","name":"G. Battista Menier","address":"Calle Seconda de la Fava, 5263, 30122 Venezia VE, Italy","latE6":45436898,"lngE6":12341097,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["2aa05f985da04bea860276b336e4668e.d",1638033697023,{"plext":{"text":"Smith82v2 destroyed a Resonator on Flame (Via Bissuola, 51, 30173 Venezia VE, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Flame (Via Bissuola, 51, 30173 Venezia VE, Italy)","name":"Flame","address":"Via Bissuola, 51, 30173 Venezia VE, Italy","latE6":45495138,"lngE6":12253059,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["cf4181e1aafe417cb3e3fe18539d776d.d",1638033163616,{"plext":{"text":"TerminateThat deployed a Resonator on Capitello a Maria delle Rose (Via Tresievoli, 44, 30034 Ca' Semenzato VE, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"TerminateThat","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Capitello a Maria delle Rose (Via Tresievoli, 44, 30034 Ca' Semenzato VE, Italy)","name":"Capitello a Maria delle Rose","address":"Via Tresievoli, 44, 30034 Ca' Semenzato VE, Italy","latE6":45464655,"lngE6":12139268,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["4082bdffd94240d79fe449b82dfe7d35.d",1638033163616,{"plext":{"text":"TerminateThat captured Capitello a Maria delle Rose (Via Tresievoli, 44, 30034 Ca' Semenzato VE, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"TerminateThat","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Capitello a Maria delle Rose (Via Tresievoli, 44, 30034 Ca' Semenzato VE, Italy)","name":"Capitello a Maria delle Rose","address":"Via Tresievoli, 44, 30034 Ca' Semenzato VE, Italy","latE6":45464655,"lngE6":12139268,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["4c9888eff79a4a62b92202bfd6ae2579.d",1638033150455,{"plext":{"text":"TerminateThat destroyed a Resonator on Capitello a Maria delle Rose (Via Tresievoli, 44, 30034 Ca' Semenzato VE, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"TerminateThat","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Capitello a Maria delle Rose (Via Tresievoli, 44, 30034 Ca' Semenzato VE, Italy)","name":"Capitello a Maria delle Rose","address":"Via Tresievoli, 44, 30034 Ca' Semenzato VE, Italy","latE6":45464655,"lngE6":12139268,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["7029498f8f074b369744f7e4a595b265.d",1638032899501,{"plext":{"text":"Smith82v2 linked Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy) to Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy)","name":"Altare Via Bissuola","address":"Via Bissuola, 48F, 30173 Venice, Italy","latE6":45493555,"lngE6":12257134,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Mestre - Agli Eroi di Nassirya (Via Amerigo Vespucci, 30173 Venezia VE, Italy)","name":"Mestre - Agli Eroi di Nassirya","address":"Via Amerigo Vespucci, 30173 Venezia VE, Italy","latE6":45497392,"lngE6":12249866,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["725bb64ccc6b434985929c629a7ebeba.d",1638032885578,{"plext":{"text":"Smith82v2 linked Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy) to Colazione Da Tiffany (Via Bissuola, 21, 30173 Venezia, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy)","name":"Altare Via Bissuola","address":"Via Bissuola, 48F, 30173 Venice, Italy","latE6":45493555,"lngE6":12257134,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Colazione Da Tiffany (Via Bissuola, 21, 30173 Venezia, Italy)","name":"Colazione Da Tiffany","address":"Via Bissuola, 21, 30173 Venezia, Italy","latE6":45495314,"lngE6":12250504,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["7a956ab3d23e4e13a2b85a52f99aaf9e.d",1638032862532,{"plext":{"text":"Smith82v2 linked Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy) to Mestre - La Nave (Viale Ancona, 26, 30172 Venice, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy)","name":"Altare Via Bissuola","address":"Via Bissuola, 48F, 30173 Venice, Italy","latE6":45493555,"lngE6":12257134,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Mestre - La Nave (Viale Ancona, 26, 30172 Venice, Italy)","name":"Mestre - La Nave","address":"Viale Ancona, 26, 30172 Venice, Italy","latE6":45484788,"lngE6":12252364,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["56e51909e10b45099261ad3b1cf237ba.d",1638032862532,{"plext":{"text":"Smith82v2 created a Control Field @Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy) +60 MUs","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy)","name":"Altare Via Bissuola","address":"Via Bissuola, 48F, 30173 Venice, Italy","latE6":45493555,"lngE6":12257134,"team":"ENLIGHTENED"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"60"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["4c370c6d9d7d484abb903d4e360df054.d",1638032809344,{"plext":{"text":"Smith82v2 created a Control Field @Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy) +179 MUs","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy)","name":"Altare Via Bissuola","address":"Via Bissuola, 48F, 30173 Venice, Italy","latE6":45493555,"lngE6":12257134,"team":"ENLIGHTENED"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"179"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["0c48f666dcfd4a3cb9a1eadcd1de35ff.d",1638032809344,{"plext":{"text":"Smith82v2 linked Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy) to Madonnina Protettrice (Via San Don\u00e0, 9, 30174 Venice, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy)","name":"Altare Via Bissuola","address":"Via Bissuola, 48F, 30173 Venice, Italy","latE6":45493555,"lngE6":12257134,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Madonnina Protettrice (Via San Don\u00e0, 9, 30174 Venice, Italy)","name":"Madonnina Protettrice","address":"Via San Don\u00e0, 9, 30174 Venice, Italy","latE6":45504785,"lngE6":12252417,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["b2dbae5b846f456ca01bbbeb5164700a.d",1638032723222,{"plext":{"text":"bierstrich destroyed a Resonator on Palazzo Della Regione Veneto (Ponte della Costituzione, Venice, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Palazzo Della Regione Veneto (Ponte della Costituzione, Venice, Italy)","name":"Palazzo Della Regione Veneto","address":"Ponte della Costituzione, Venice, Italy","latE6":45439806,"lngE6":12320502,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["8d16749bc1154a25831d2f03692fcbea.d",1638032719489,{"plext":{"text":"bierstrich destroyed a Resonator on Immaculata Virgo (Fondamenta San Simeone Piccolo, 30135 Venice, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Immaculata Virgo (Fondamenta San Simeone Piccolo, 30135 Venice, Italy)","name":"Immaculata Virgo","address":"Fondamenta San Simeone Piccolo, 30135 Venice, Italy","latE6":45440473,"lngE6":12321050,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["37a85c6bae564110acd154c60c485363.d",1638032709450,{"plext":{"text":"bierstrich destroyed a Resonator on Venezia - Monumento Ai Ferrovieri (Ponte della Costituzione, Venice, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Venezia - Monumento Ai Ferrovieri (Ponte della Costituzione, Venice, Italy)","name":"Venezia - Monumento Ai Ferrovieri","address":"Ponte della Costituzione, Venice, Italy","latE6":45440197,"lngE6":12320917,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["2f53b807f2344df0a9f46401c90a84e1.d",1638032709450,{"plext":{"text":"bierstrich destroyed a Resonator on Venezia Railway St Statue (Sestiere Castello, 2737, 30122 Venezia, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Venezia Railway St Statue (Sestiere Castello, 2737, 30122 Venezia, Italy)","name":"Venezia Railway St Statue","address":"Sestiere Castello, 2737, 30122 Venezia, Italy","latE6":45441013,"lngE6":12319438,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["9842bfacf6cb4f14bd006d11c1f745ab.d",1638032706538,{"plext":{"text":"bierstrich destroyed a Resonator on Fresque Gare Venise (Sestiere Castello, 2737, 30122 Venezia, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Fresque Gare Venise (Sestiere Castello, 2737, 30122 Venezia, Italy)","name":"Fresque Gare Venise","address":"Sestiere Castello, 2737, 30122 Venezia, Italy","latE6":45441063,"lngE6":12319768,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["6992dca02f8b413da66a72c4d439ca8a.d",1638032706538,{"plext":{"text":"bierstrich destroyed a Resonator on At the train station (Venezia S.Lucia, 30121 Venezia, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"At the train station (Venezia S.Lucia, 30121 Venezia, Italy)","name":"At the train station","address":"Venezia S.Lucia, 30121 Venezia, Italy","latE6":45440755,"lngE6":12320247,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["4aebf901e87645e78780c4ce221883c5.d",1638032706538,{"plext":{"text":"bierstrich destroyed a Resonator on S. LUCIA (Stazione di Venezia Santa Lucia, 30121 Venezia VE, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"S. LUCIA (Stazione di Venezia Santa Lucia, 30121 Venezia VE, Italy)","name":"S. LUCIA","address":"Stazione di Venezia Santa Lucia, 30121 Venezia VE, Italy","latE6":45440701,"lngE6":12320542,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["3386ae6e794343ea8d4ef0110904d374.d",1638032630326,{"plext":{"text":"Smith82v2 linked Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy) to Gelateria Al Parco (Via Casona, 3-21, 30173 Venice, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy)","name":"Altare Via Bissuola","address":"Via Bissuola, 48F, 30173 Venice, Italy","latE6":45493555,"lngE6":12257134,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Gelateria Al Parco (Via Casona, 3-21, 30173 Venice, Italy)","name":"Gelateria Al Parco","address":"Via Casona, 3-21, 30173 Venice, Italy","latE6":45496987,"lngE6":12265025,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["0947436d392a47bab965df5b7370c157.d",1638032630326,{"plext":{"text":"Smith82v2 created a Control Field @Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy) +63 MUs","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Smith82v2","team":"ENLIGHTENED"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Altare Via Bissuola (Via Bissuola, 48F, 30173 Venice, Italy)","name":"Altare Via Bissuola","address":"Via Bissuola, 48F, 30173 Venice, Italy","latE6":45493555,"lngE6":12257134,"team":"ENLIGHTENED"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"63"}],["TEXT",{"plain":" MUs"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["1c2d9f27d9f342ee839844acffb59a35.d",1638032420035,{"plext":{"text":"Alexcl4sh95 captured Giostrine Via Parco Ferroviario (Via Case Nuove, 41, 30175 Venezia VE, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Alexcl4sh95","team":"ENLIGHTENED"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Giostrine Via Parco Ferroviario (Via Case Nuove, 41, 30175 Venezia VE, Italy)","name":"Giostrine Via Parco Ferroviario","address":"Via Case Nuove, 41, 30175 Venezia VE, Italy","latE6":45479807,"lngE6":12212061,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["10c9972cf92246098917522cd4f9cce6.d",1638032420035,{"plext":{"text":"Alexcl4sh95 deployed a Resonator on Giostrine Via Parco Ferroviario (Via Case Nuove, 41, 30175 Venezia VE, Italy)","team":"ENLIGHTENED","markup":[["PLAYER",{"plain":"Alexcl4sh95","team":"ENLIGHTENED"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Giostrine Via Parco Ferroviario (Via Case Nuove, 41, 30175 Venezia VE, Italy)","name":"Giostrine Via Parco Ferroviario","address":"Via Case Nuove, 41, 30175 Venezia VE, Italy","latE6":45479807,"lngE6":12212061,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["b910fdc766c7400597c5e79b78b6ef68.d",1638032341821,{"plext":{"text":"bierstrich captured Venezia - Mosaico Stazione (Calle Carmelitani, 30121 Venice, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Venezia - Mosaico Stazione (Calle Carmelitani, 30121 Venice, Italy)","name":"Venezia - Mosaico Stazione","address":"Calle Carmelitani, 30121 Venice, Italy","latE6":45440968,"lngE6":12320921,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["989424117aa34ee4ae5dbb0fe913de7e.d",1638032341821,{"plext":{"text":"bierstrich deployed a Resonator on Venezia - Mosaico Stazione (Calle Carmelitani, 30121 Venice, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Venezia - Mosaico Stazione (Calle Carmelitani, 30121 Venice, Italy)","name":"Venezia - Mosaico Stazione","address":"Calle Carmelitani, 30121 Venice, Italy","latE6":45440968,"lngE6":12320921,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["56234a530d8948db9a2dee70b06e225a.d",1638032128087,{"plext":{"text":"bierstrich captured Stazione FS S. Lucia (Ponte della Costituzione, Venice, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Stazione FS S. Lucia (Ponte della Costituzione, Venice, Italy)","name":"Stazione FS S. Lucia","address":"Ponte della Costituzione, Venice, Italy","latE6":45440930,"lngE6":12321245,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["4573cf6ba8b74fc29753a9f4197ded67.d",1638032128087,{"plext":{"text":"bierstrich deployed a Resonator on Stazione FS S. Lucia (Ponte della Costituzione, Venice, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Stazione FS S. Lucia (Ponte della Costituzione, Venice, Italy)","name":"Stazione FS S. Lucia","address":"Ponte della Costituzione, Venice, Italy","latE6":45440930,"lngE6":12321245,"team":"RESISTANCE"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["208bbff762ab466694c58a5362baf1d8.d",1638032123545,{"plext":{"text":"bierstrich destroyed a Resonator on Venezia - Monumento Ai Ferrovieri (Ponte della Costituzione, Venice, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Venezia - Monumento Ai Ferrovieri (Ponte della Costituzione, Venice, Italy)","name":"Venezia - Monumento Ai Ferrovieri","address":"Ponte della Costituzione, Venice, Italy","latE6":45440197,"lngE6":12320917,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["f431e2c3aa704a5c961e396268526f2d.d",1638032122246,{"plext":{"text":"bierstrich destroyed a Resonator on At the train station (Venezia S.Lucia, 30121 Venezia, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"At the train station (Venezia S.Lucia, 30121 Venezia, Italy)","name":"At the train station","address":"Venezia S.Lucia, 30121 Venezia, Italy","latE6":45440755,"lngE6":12320247,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["6752283fde95459db80a56d0926b3098.d",1638032116630,{"plext":{"text":"bierstrich destroyed a Resonator on S. LUCIA (Stazione di Venezia Santa Lucia, 30121 Venezia VE, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"S. LUCIA (Stazione di Venezia Santa Lucia, 30121 Venezia VE, Italy)","name":"S. LUCIA","address":"Stazione di Venezia Santa Lucia, 30121 Venezia VE, Italy","latE6":45440701,"lngE6":12320542,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["66c1dab04bfe4624afe65bd42579270f.d",1638032116630,{"plext":{"text":"bierstrich destroyed a Resonator on Santa Maria Di Nazareth (Fondamenta Scalzi Cannaregio, 30121 Venice, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Santa Maria Di Nazareth (Fondamenta Scalzi Cannaregio, 30121 Venice, Italy)","name":"Santa Maria Di Nazareth","address":"Fondamenta Scalzi Cannaregio, 30121 Venice, Italy","latE6":45441268,"lngE6":12322171,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["4f13dc050d2b43d597c40f6b25a17a5e.d",1638032116630,{"plext":{"text":"bierstrich destroyed a Resonator on Immaculata Virgo (Fondamenta San Simeone Piccolo, 30135 Venice, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Immaculata Virgo (Fondamenta San Simeone Piccolo, 30135 Venice, Italy)","name":"Immaculata Virgo","address":"Fondamenta San Simeone Piccolo, 30135 Venice, Italy","latE6":45440473,"lngE6":12321050,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}],["41dfed473a4d40e0982a7b0983e2a98b.d",1638032116630,{"plext":{"text":"bierstrich destroyed a Resonator on Stazione FS S. Lucia (Ponte della Costituzione, Venice, Italy)","team":"RESISTANCE","markup":[["PLAYER",{"plain":"bierstrich","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Stazione FS S. Lucia (Ponte della Costituzione, Venice, Italy)","name":"Stazione FS S. Lucia","address":"Ponte della Costituzione, Venice, Italy","latE6":45440930,"lngE6":12321245,"team":"ENLIGHTENED"}]],"plextType":"SYSTEM_BROADCAST","categories":1}}]]}"#,
            r#"{"result":[["70f945bad9c647778f309e626f98e827.d", 1638824370147, {"plext":{"text":"Dohko88 linked Portale Ingresso Filanda Motta (Via Chiesa Campocroce, 7, 31021 Campocroce Treviso, Italy) to Marcon - Area giochi via Don Ballan (Via Don Ballan, 34G, 30020 Marcon-Gaggio-Colmello VE, Italy)", "team": "ENLIGHTENED", "markup": [["PLAYER", { "plain": "Dohko88", "team": "ENLIGHTENED" }], ["TEXT", { "plain": " linked " }], ["PORTAL", { "plain": "Portale Ingresso Filanda Motta (Via Chiesa Campocroce, 7, 31021 Campocroce Treviso, Italy)", "name": "Portale Ingresso Filanda Motta", "address": "Via Chiesa Campocroce, 7, 31021 Campocroce Treviso, Italy", "latE6": 45585547, "lngE6": 12220264, "team": "ENLIGHTENED" }], ["TEXT", { "plain": " to " }], ["PORTAL", { "plain": "Marcon - Area giochi via Don Ballan (Via Don Ballan, 34G, 30020 Marcon-Gaggio-Colmello VE, Italy)", "name": "Marcon - Area giochi via Don Ballan", "address": "Via Don Ballan, 34G, 30020 Marcon-Gaggio-Colmello VE, Italy", "latE6": 45568374, "lngE6": 12301930, "team": "ENLIGHTENED" }]], "plextType": "SYSTEM_BROADCAST", "categories": 1 }}]]}"#,
        ];
        for s in inputs {
            let res: ingress_intel_rs::plexts::IntelResponse = serde_json::from_str(s).unwrap();
            let config = crate::config::get().await.unwrap();

            let plexts = res
                .result
                .iter()
                .rev()
                .filter_map(|(_id, _time, plext)| {
                    let msg_type = crate::entities::PlextType::from(plext.plext.markup.as_slice());
                    crate::entities::Plext::try_from((msg_type, &plext.plext)).ok()
                })
                .collect::<Vec<_>>();
            let len = plexts.len();
            let msgs = crate::dedup_flatten::windows_dedup_flatten(plexts, 8);
            assert_eq!(len, msgs.len());
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
