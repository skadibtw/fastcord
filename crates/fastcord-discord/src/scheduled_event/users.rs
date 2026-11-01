//! Manual, bounded pages of interested members. Public read contract only;
//! interoperability with a user account has not been verified.
use super::*;

pub const EVENT_USERS_PAGE: usize = 50;
const MAX_USERS_BODY: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventUser {
    pub id: Snowflake,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventUsers {
    pub rows: Vec<EventUser>,
    pub next: Option<Snowflake>,
}

fn parse(body: &[u8], event: Snowflake, after: Option<Snowflake>) -> Option<EventUsers> {
    if body.len() > MAX_USERS_BODY {
        return None;
    }
    let raw: Vec<Value> = serde_json::from_slice(body).ok()?;
    if raw.len() > EVENT_USERS_PAGE {
        return None;
    }
    let full = raw.len() == EVENT_USERS_PAGE;
    let mut rows = Vec::with_capacity(raw.len());
    for raw in raw {
        let held: Snowflake =
            serde_json::from_value(raw.get("guild_scheduled_event_id")?.clone()).ok()?;
        if held != event {
            return None;
        }
        let user = raw.get("user")?;
        let id: Snowflake = serde_json::from_value(user.get("id")?.clone()).ok()?;
        if id.0 == 0 || after.is_some_and(|after| id <= after) {
            return None;
        }
        let name = user
            .get("global_name")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| user.get("username").and_then(Value::as_str))?;
        if rows.iter().any(|row: &EventUser| row.id == id) {
            continue;
        }
        rows.push(EventUser {
            id,
            name: name.chars().take(80).collect(),
        });
    }
    rows.sort_by_key(|row| row.id);
    let next = full.then(|| rows.last().map(|row| row.id)).flatten();
    Some(EventUsers { rows, next })
}

fn users_request(
    guild: Snowflake,
    event: Snowflake,
    after: Option<Snowflake>,
) -> Result<RestRequest, RestError> {
    let mut path = format!(
        "/guilds/{guild}/scheduled-events/{event}/users?limit={EVENT_USERS_PAGE}&with_member=false"
    );
    if let Some(after) = after {
        path.push_str(&format!("&after={after}"));
    }
    Ok(RestRequest::new(Route::new(Method::GET, &path)?, Priority::UserRead).with_error_detail())
}

impl<C: Clock> RestClient<C> {
    /// One page per explicit click; no automatic pagination or member cache.
    pub async fn scheduled_event_users(
        &self,
        guild: Snowflake,
        event: Snowflake,
        after: Option<Snowflake>,
    ) -> Result<EventUsers, RestError> {
        let response = self.execute(users_request(guild, event, after)?).await?;
        parse(response.body(), event, after).ok_or(RestError::InvalidJson)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(user: u64) -> Value {
        json!({"guild_scheduled_event_id":"7", "user":{"id":user.to_string(),"username":"member","global_name":null}})
    }
    #[test]
    fn pages_are_bounded_and_cursor_advances_without_accumulating_members() {
        let raw: Vec<_> = (1..=50).map(row).collect();
        let page = parse(&serde_json::to_vec(&raw).unwrap(), Snowflake(7), None).unwrap();
        assert_eq!(page.rows.len(), 50);
        assert_eq!(page.next, Some(Snowflake(50)));
        assert!(
            parse(
                &serde_json::to_vec(&raw).unwrap(),
                Snowflake(7),
                Some(Snowflake(50))
            )
            .is_none()
        );
        assert!(
            parse(
                &serde_json::to_vec(&vec![row(1); 51]).unwrap(),
                Snowflake(7),
                None
            )
            .is_none()
        );
        assert!(parse(&vec![b' '; MAX_USERS_BODY + 1], Snowflake(7), None).is_none());
        assert_eq!(
            parse(b"[]", Snowflake(7), Some(Snowflake(50)))
                .unwrap()
                .next,
            None
        );
    }
    #[test]
    fn malformed_wrong_event_and_duplicate_rows_never_leak_or_stall() {
        assert!(
            parse(
                &serde_json::to_vec(&vec![row(1)]).unwrap(),
                Snowflake(8),
                None
            )
            .is_none()
        );
        assert!(parse(b"[{}]", Snowflake(7), None).is_none());
        let mut long = row(1);
        long["user"]["global_name"] = json!("😀".repeat(1000));
        let page = parse(
            &serde_json::to_vec(&vec![long.clone(), long]).unwrap(),
            Snowflake(7),
            None,
        )
        .unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].name.len(), 320);
        assert_eq!(page.next, None);
    }
}
