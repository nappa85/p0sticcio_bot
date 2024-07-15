use sea_orm::{ActiveModelTrait, ActiveValue, ConnectionTrait, EntityTrait};
use tgbot::types::UserPeerId;

use super::Error;

use crate::database::location;

pub async fn set<C>(conn: &C, user_id: UserPeerId, latitude: f32, longitude: f32) -> Result<(), Error>
where
    C: ConnectionTrait,
{
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
