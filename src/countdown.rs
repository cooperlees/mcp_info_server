use serde::{Deserialize, Serialize};

use crate::state::{AppError, AppState};

#[derive(Debug, Clone, Deserialize)]
struct CountdownApiResponse {
    generated_at: String,
    timezone: String,
    countdowns: Vec<CountdownRaw>,
}

#[derive(Debug, Clone, Deserialize)]
struct CountdownRaw {
    slug: String,
    title: String,
    target_timestamp: i64,
    target_datetime: String,
    seconds_remaining: i64,
    breakdown: CountdownBreakdown,
    human: String,
    started: bool,
    permalink: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CountdownBreakdown {
    pub years: i64,
    pub days: i64,
    pub hours: i64,
    pub minutes: i64,
    pub seconds: i64,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Countdown {
    pub slug: String,
    pub title: String,
    pub target_timestamp: i64,
    pub target_datetime: String,
    pub seconds_remaining: i64,
    pub breakdown: CountdownBreakdown,
    pub human: String,
    pub started: bool,
    pub permalink: String,
}

impl From<CountdownRaw> for Countdown {
    fn from(raw: CountdownRaw) -> Self {
        Self {
            slug: raw.slug,
            title: raw.title,
            target_timestamp: raw.target_timestamp,
            target_datetime: raw.target_datetime,
            seconds_remaining: raw.seconds_remaining,
            breakdown: raw.breakdown,
            human: raw.human,
            started: raw.started,
            permalink: raw.permalink,
        }
    }
}

// The MCP spec requires a tool's output schema to be rooted at an `object`,
// not an `array` — mirrors `PostListResult`/`PageListResult` in mcp_server.rs.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CountdownList {
    pub generated_at: String,
    pub timezone: String,
    pub countdowns: Vec<Countdown>,
}

#[tracing::instrument(level = "debug", skip(state))]
async fn fetch(state: &AppState) -> Result<CountdownApiResponse, AppError> {
    let body = state.fetch_countdown_cached(&state.countdown_url).await?;
    Ok(serde_json::from_str(&body)?)
}

pub async fn list_countdowns(state: &AppState) -> Result<CountdownList, AppError> {
    let response = fetch(state).await?;
    Ok(CountdownList {
        generated_at: response.generated_at,
        timezone: response.timezone,
        countdowns: response
            .countdowns
            .into_iter()
            .map(Countdown::from)
            .collect(),
    })
}

pub async fn get_countdown(state: &AppState, slug: &str) -> Result<Countdown, AppError> {
    let response = fetch(state).await?;
    response
        .countdowns
        .into_iter()
        .find(|c| c.slug == slug)
        .map(Countdown::from)
        .ok_or_else(|| AppError::NotFound(format!("no countdown found with slug {slug:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_body() -> String {
        r#"{
            "generated_at": "2026-07-27T14:41:25-05:00",
            "timezone": "US/Central",
            "events": [{"slug": "ignored", "title": "ignored", "timestamp": 0, "datetime": "ignored"}],
            "countdowns": [
                {
                    "slug": "london-cotswolds-trip",
                    "title": "London / Cotswolds Trip",
                    "target_timestamp": 1785549600,
                    "target_datetime": "2026-07-31T21:00:00-05:00",
                    "seconds_remaining": 368315,
                    "breakdown": {"years": 0, "days": 4, "hours": 6, "minutes": 18, "seconds": 35},
                    "human": "4d to go",
                    "started": false,
                    "permalink": "https://countdown.cooperlees.com/?event=london-cotswolds-trip"
                },
                {
                    "slug": "41st-birthday",
                    "title": "41st Birthday",
                    "target_timestamp": 1792040400,
                    "target_datetime": "2026-10-15T00:00:00-05:00",
                    "seconds_remaining": 6859115,
                    "breakdown": {"years": 0, "days": 79, "hours": 9, "minutes": 18, "seconds": 35},
                    "human": "79d to go",
                    "started": false,
                    "permalink": "https://countdown.cooperlees.com/?event=41st-birthday"
                }
            ]
        }"#
        .to_owned()
    }

    #[tokio::test]
    async fn list_countdowns_parses_all_events_in_order() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/")
            .match_header("accept", "application/json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(sample_body())
            .create_async()
            .await;
        let state = AppState::new(
            "unused".to_owned(),
            "unused".to_owned(),
            server.url(),
            "unused".to_owned(),
        )
        .unwrap();

        let list = list_countdowns(&state).await.unwrap();

        assert_eq!(list.timezone, "US/Central");
        assert_eq!(list.countdowns.len(), 2);
        assert_eq!(list.countdowns[0].slug, "london-cotswolds-trip");
        assert_eq!(list.countdowns[1].slug, "41st-birthday");
        assert_eq!(list.countdowns[0].breakdown.days, 4);
    }

    #[tokio::test]
    async fn get_countdown_returns_matching_event() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(sample_body())
            .create_async()
            .await;
        let state = AppState::new(
            "unused".to_owned(),
            "unused".to_owned(),
            server.url(),
            "unused".to_owned(),
        )
        .unwrap();

        let countdown = get_countdown(&state, "41st-birthday").await.unwrap();

        assert_eq!(countdown.title, "41st Birthday");
        assert_eq!(countdown.human, "79d to go");
    }

    #[tokio::test]
    async fn get_countdown_returns_not_found_for_unknown_slug() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(sample_body())
            .create_async()
            .await;
        let state = AppState::new(
            "unused".to_owned(),
            "unused".to_owned(),
            server.url(),
            "unused".to_owned(),
        )
        .unwrap();

        let err = get_countdown(&state, "missing").await.unwrap_err();

        assert!(matches!(err, AppError::NotFound(_)));
    }
}
