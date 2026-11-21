use sea_orm::{ActiveModelTrait, ActiveValue, ConnectionTrait, EntityTrait};
use super::{Context, Error};

use crate::database::location;

pub async fn set<C>(ctx: &Context<'_, C>, latitude: f32, longitude: f32) -> Result<(), Error>
where
    C: ConnectionTrait,
{
    let Context { conn, user_id, .. } = *ctx;
    let exists = location::Entity::find_by_id(i64::from(user_id)).one(conn).await?.is_some();

    let am = location::ActiveModel {
        user_id: ActiveValue::Set(i64::from(user_id)),
        latitude: ActiveValue::Set(f64::from(latitude)),
        longitude: ActiveValue::Set(f64::from(longitude)),
    };

    if exists {
        am.update(conn).await?;
    } else {
        am.insert(conn).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use sea_orm::{EntityTrait, PaginatorTrait};
    use tgbot::{
        api::Client,
        types::{ChatPeerId, UserPeerId},
    };

    use super::*;
    use crate::database::testing::db;

    #[tokio::test]
    async fn insert_then_update() {
        let Some(db) = db().await else { return };
        let client = Client::new("0:test").unwrap();
        let (portal_tx, _portal_rx) = crate::scan_queue();
        let ctx = Context {
            conn: &db.conn,
            client: &client,
            portal_tx: &portal_tx,
            chat_id: ChatPeerId::from(7),
            user_id: UserPeerId::from(7),
            message_id: 1,
        };

        set(&ctx, 45.5, 11.25).await.unwrap();
        let row = location::Entity::find_by_id(7).one(&db.conn).await.unwrap();
        assert_eq!(row, Some(location::Model { user_id: 7, latitude: 45.5, longitude: 11.25 }));

        set(&ctx, -12.125, 100.75).await.unwrap();
        let row = location::Entity::find_by_id(7).one(&db.conn).await.unwrap();
        assert_eq!(row, Some(location::Model { user_id: 7, latitude: -12.125, longitude: 100.75 }));
        assert_eq!(location::Entity::find().count(&db.conn).await.unwrap(), 1);
    }
}
