//! polls the portals hit by a virus, see `database::portal_watch`

use std::{collections::HashMap, sync::atomic::Ordering, time::Duration};

use chrono::Utc;
use sea_orm::{ConnectionTrait, DbErr};
use smol_str::SmolStr;
use tokio::{
    sync::mpsc::{self, error::TryRecvError},
    time::{self, MissedTickBehavior},
};
use tracing::{debug, error, info, warn};

use crate::{
    PortalOrCoords, SCANNING, ScanQueue, ScanUpdate,
    database::{portal, portal_watch},
};

const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// scans every minute the portals hit by a virus, after the cooldown, to record resonators upgraded
/// by accounts of the other faction before a second virus brings everything back to a single owner
pub async fn virus_watcher<C>(conn: &C, portal_tx: ScanQueue)
where
    C: ConnectionTrait,
{
    // scans requested and not done yet, by portal id
    let mut pending = HashMap::new();
    let mut interval = time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        // the nightly scan already keeps Intel busy, polling too would only make things worse
        if SCANNING.load(Ordering::Relaxed) {
            continue;
        }
        if let Err(err) = tick(conn, &portal_tx, &mut pending, Utc::now().timestamp_millis()).await {
            error!("Portal watch error: {err}");
        }
    }
}

/// queues a scan of every watched portal, unless its previous one is still waiting
async fn tick<C>(
    conn: &C,
    portal_tx: &ScanQueue,
    pending: &mut HashMap<SmolStr, mpsc::UnboundedReceiver<ScanUpdate>>,
    now: i64,
) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
    // the scanner drops the reply sender once done
    pending.retain(|_, rx| matches!(rx.try_recv(), Err(TryRecvError::Empty)));

    for watch in portal_watch::active(conn, now).await? {
        let Some(portal) = portal::Entity::find_by_coords(conn, watch.latitude, watch.longitude).await? else {
            warn!("Watched portal at {},{} not found", watch.latitude, watch.longitude);
            continue;
        };
        if let Some(revision) = portal::Entity::get_latest_revision(conn, &portal.portal_id).await? {
            match watch.faction {
                None => portal_watch::set_faction(conn, watch.id, revision.faction).await?,
                Some(faction) if faction != revision.faction => {
                    info!("Watched portal {} changed faction, watch ended", portal.portal_id);
                    portal_watch::end(conn, watch.id, now).await?;
                    continue;
                }
                Some(_) => {}
            }
        }

        let id = SmolStr::new(&portal.portal_id);
        if pending.contains_key(&id) {
            debug!("Watched portal {id} previous scan still waiting");
            continue;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        if let Err(err) = portal_tx.send((PortalOrCoords::Watch { id: id.clone() }, now, Some(tx))) {
            error!("Portal scan queue error: {err}");
            return Ok(());
        }
        pending.insert(id, rx);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sea_orm::EntityTrait;

    use super::*;
    use crate::{
        ScanReceiver,
        database::{
            portal_revision::Faction,
            portal_watch::{COOLDOWN, on_virus},
            testing::{Spec, db, scan},
        },
        scan_queue,
    };

    const T: i64 = 1_782_864_000_000;
    const MIN: i64 = 60_000;
    const FONTANA: Spec<'static> = Spec {
        title: "Fontana",
        faction: "R",
        lat: 45.1,
        lon: 11.1,
        level: 8,
        health: 100,
        owner: Some("cheater"),
        resonators: &[],
        mods: &[],
    };

    /// the watched portals ids just queued
    fn queued(rx: &mut ScanReceiver) -> Vec<(PortalOrCoords, i64, Option<crate::ScanReply>)> {
        std::iter::from_fn(|| rx.try_recv()).collect()
    }

    fn ids(requests: &[(PortalOrCoords, i64, Option<crate::ScanReply>)]) -> Vec<&str> {
        requests
            .iter()
            .map(|request| match &request.0 {
                PortalOrCoords::Watch { id } => id.as_str(),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn polling() {
        let Some(db) = db().await else { return };
        let conn = &db.conn;
        let (portal_tx, mut portal_rx) = scan_queue();
        let mut pending = HashMap::new();

        scan(conn, "aa.1", T, FONTANA).await;
        scan(conn, "bb.1", T, Spec { title: "Chiesa", lat: 45.2, ..FONTANA }).await;
        on_virus(conn, 45.1, 11.1, T).await.unwrap();
        on_virus(conn, 45.2, 11.1, T + 30 * MIN).await.unwrap();
        // a virus on a portal not stored yet
        on_virus(conn, 45.9, 11.9, T).await.unwrap();

        // cooldown
        tick(conn, &portal_tx, &mut pending, T + 59 * MIN).await.unwrap();
        assert!(queued(&mut portal_rx).is_empty());

        let now = T + COOLDOWN;
        tick(conn, &portal_tx, &mut pending, now).await.unwrap();
        let requests = queued(&mut portal_rx);
        assert_eq!(ids(&requests), ["aa.1"]);
        assert_eq!(requests[0].1, now);
        let watch = portal_watch::Entity::find_by_id(1).one(conn).await.unwrap().unwrap();
        assert_eq!(watch.faction, Some(Faction::Resistance));

        // the previous scan is still waiting
        tick(conn, &portal_tx, &mut pending, now + MIN).await.unwrap();
        assert!(queued(&mut portal_rx).is_empty());
        // done
        drop(requests);
        tick(conn, &portal_tx, &mut pending, now + 2 * MIN).await.unwrap();
        // kept, so the scan is still waiting
        let aa = queued(&mut portal_rx);
        assert_eq!(ids(&aa), ["aa.1"]);

        // the second portal past its cooldown too, the first one is still pending
        tick(conn, &portal_tx, &mut pending, T + 95 * MIN).await.unwrap();
        let bb = queued(&mut portal_rx);
        assert_eq!(ids(&bb), ["bb.1"]);

        // second virus recorded by a scan before the comm shows it
        drop(aa);
        scan(conn, "aa.1", T + 100 * MIN, Spec { faction: "E", ..FONTANA }).await;
        tick(conn, &portal_tx, &mut pending, T + 101 * MIN).await.unwrap();
        // bb.1 is still pending
        assert!(queued(&mut portal_rx).is_empty());
        let watch = portal_watch::Entity::find_by_id(1).one(conn).await.unwrap().unwrap();
        assert_eq!(watch.ended_at, Some(T + 101 * MIN));
        tick(conn, &portal_tx, &mut pending, T + 102 * MIN).await.unwrap();
        assert!(queued(&mut portal_rx).is_empty());
        drop(bb);
    }
}
