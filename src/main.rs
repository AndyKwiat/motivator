//! Motivate: three quotes a day + an "I'm grateful" button, counting down to recovery day.
//!
//! Env: PORT, DATA_DIR (sqlite lives there), STATIC_DIR, START_DATE / GOAL_DATE (YYYY-MM-DD).
//! Identity comes from the gate's X-Auth-Email header. Days are numbered from 1 (START_DATE)
//! and computed in the browser's timezone (`tz` = minutes east of UTC); future days are never served.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{
    env,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tower_http::services::ServeDir;

const QUOTES: &str = include_str!("../quotes.txt");
const KINDS: [&str; 4] = ["q0", "q1", "q2", "grateful"];

#[derive(Serialize, Clone)]
struct Quote {
    cat: &'static str,
    author: &'static str,
    text: &'static str,
}

struct St {
    db: Mutex<Connection>,
    start: i64, // days since epoch
    goal: i64,
    days: Vec<[Quote; 3]>, // index 0 = day 1
}
type S = State<Arc<St>>;

// ---------- dates ----------

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.trim().split('-').map(|p| p.parse::<i64>().ok());
    Some(days_from_civil(it.next()??, it.next()??, it.next()??))
}

fn fmt_date(z: i64) -> String {
    let (y, m, d) = civil_from_days(z);
    format!("{y:04}-{m:02}-{d:02}")
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

/// Local calendar day (days since epoch) for a UTC instant and a tz offset in minutes east of UTC.
fn local_day(now_ms: i64, tz_min: i64) -> i64 {
    let tz = tz_min.clamp(-14 * 60, 14 * 60);
    (now_ms + tz * 60_000).div_euclid(86_400_000)
}

fn iso_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (day, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    format!("{}T{:02}:{:02}:{:02}Z", fmt_date(day), rem / 3600, rem % 3600 / 60, rem % 60)
}

// ---------- quotes ----------

fn shuffled(mut v: Vec<Quote>, seed: u64) -> Vec<Quote> {
    let mut s = seed;
    for i in (1..v.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        v.swap(i, (s % (i as u64 + 1)) as usize);
    }
    v
}

/// One "get motivated", one "it'll be ok", one "timeless wisdom" quote per day; cycles if the plan outlasts the list.
fn plan_days(n: usize) -> Vec<[Quote; 3]> {
    let all: Vec<Quote> = QUOTES
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut p = l.splitn(3, '|');
            Some(Quote { cat: p.next()?, author: p.next()?, text: p.next()? })
        })
        .collect();
    let cat = |c: &str, seed| shuffled(all.iter().filter(|q| q.cat == c).cloned().collect(), seed);
    let (g, k, w) = (cat("g", 0x9E3779B97F4A7C15), cat("k", 0xD1B54A32D192ED03), cat("w", 0x8CB92BA72F3D8DD7));
    (0..n).map(|i| [g[i % g.len()].clone(), k[i % k.len()].clone(), w[i % w.len()].clone()]).collect()
}

// ---------- handlers ----------

fn email(h: &HeaderMap) -> String {
    h.get("x-auth-email").and_then(|v| v.to_str().ok()).unwrap_or("local").to_lowercase()
}

/// 1-based day number of "today", capped to the plan length.
fn today_num(st: &St, tz: i64) -> i64 {
    (local_day(now_ms(), tz) - st.start + 1).min(st.days.len() as i64)
}

fn acks_by_day(st: &St, who: &str) -> std::collections::HashMap<i64, Map<String, Value>> {
    let db = st.db.lock().unwrap();
    let mut q = db.prepare("SELECT day, kind, at FROM acks WHERE email = ?1").unwrap();
    let mut out = std::collections::HashMap::<i64, Map<String, Value>>::new();
    let rows = q
        .query_map(params![who], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
        .unwrap();
    for (day, kind, at) in rows.flatten() {
        out.entry(day).or_default().insert(kind, Value::String(at));
    }
    out
}

#[derive(Deserialize)]
struct TzQ {
    #[serde(default)]
    tz: i64,
}

async fn state(State(st): S, h: HeaderMap, Query(q): Query<TzQ>) -> Json<Value> {
    let who = email(&h);
    let today = today_num(&st, q.tz);
    let mut acks = acks_by_day(&st, &who);
    let days: Vec<Value> = (1..=today.max(0))
        .map(|d| {
            json!({
                "day": d,
                "date": fmt_date(st.start + d - 1),
                "quotes": st.days[(d - 1) as usize],
                "acks": acks.remove(&d).unwrap_or_default(),
            })
        })
        .collect();
    Json(json!({
        "email": who,
        "start": fmt_date(st.start),
        "goal": fmt_date(st.goal),
        "total_days": st.days.len(),
        "today": today,
        "days": days,
    }))
}

#[derive(Deserialize)]
struct AckBody {
    day: i64,
    kind: String,
    #[serde(default)]
    tz: i64,
}

async fn ack(State(st): S, h: HeaderMap, Json(b): Json<AckBody>) -> Response {
    if !KINDS.contains(&b.kind.as_str()) {
        return (StatusCode::BAD_REQUEST, "bad kind").into_response();
    }
    if b.day < 1 || b.day > today_num(&st, b.tz) {
        return (StatusCode::FORBIDDEN, "no peeking at the future!").into_response();
    }
    let who = email(&h);
    st.db
        .lock()
        .unwrap()
        .execute(
            "INSERT OR IGNORE INTO acks (email, day, kind, at) VALUES (?1, ?2, ?3, ?4)",
            params![who, b.day, b.kind, iso_utc(now_ms())],
        )
        .unwrap();
    let acks = acks_by_day(&st, &who).remove(&b.day).unwrap_or_default();
    Json(json!({ "day": b.day, "acks": acks })).into_response()
}

fn open_db(path: &str) -> Connection {
    let c = Connection::open(path).expect("open sqlite");
    c.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;
         CREATE TABLE IF NOT EXISTS acks (
            email TEXT NOT NULL, day INTEGER NOT NULL, kind TEXT NOT NULL, at TEXT NOT NULL,
            PRIMARY KEY (email, day, kind)
         );",
    )
    .expect("schema");
    c
}

fn build_app(st: Arc<St>, static_dir: PathBuf) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/state", get(state))
        .route("/api/ack", post(ack))
        .fallback_service(ServeDir::new(static_dir))
        .with_state(st)
}

fn new_state(db: Connection, start: &str, goal: &str) -> St {
    let (start, goal) = (parse_date(start).expect("START_DATE"), parse_date(goal).expect("GOAL_DATE"));
    St { db: Mutex::new(db), start, goal, days: plan_days((goal - start + 1).max(1) as usize) }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .without_time()
        .init();
    let data_dir = env::var("DATA_DIR").unwrap_or_else(|_| ".".into());
    let static_dir = PathBuf::from(env::var("STATIC_DIR").unwrap_or_else(|_| "./static".into()));
    let start = env::var("START_DATE").unwrap_or_else(|_| "2026-10-18".into());
    let goal = env::var("GOAL_DATE").unwrap_or_else(|_| "2027-01-23".into());
    let port: u16 = env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3000);

    let st = Arc::new(new_state(open_db(&format!("{data_dir}/motivate.sqlite")), &start, &goal));
    tracing::info!("plan {start} -> {goal}: {} days, static {}", st.days.len(), static_dir.display());
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    tracing::info!("motivate listening on {addr}");
    axum::serve(listener, build_app(st, static_dir)).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app_starting_days_ago(n: i64) -> Router {
        let today = local_day(now_ms(), 0);
        let st = new_state(open_db(":memory:"), &fmt_date(today - n), &fmt_date(today + 90));
        build_app(Arc::new(st), PathBuf::from("static"))
    }

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
        let r = app.clone().oneshot(req).await.unwrap();
        let s = r.status();
        let b = r.into_body().collect().await.unwrap().to_bytes();
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    fn ack_req(day: i64, kind: &str) -> Request<Body> {
        Request::post("/api/ack")
            .header("content-type", "application/json")
            .header("x-auth-email", "test@example.com")
            .body(Body::from(json!({"day": day, "kind": kind, "tz": 0}).to_string()))
            .unwrap()
    }

    fn state_req() -> Request<Body> {
        Request::get("/api/state?tz=0").header("x-auth-email", "test@example.com").body(Body::empty()).unwrap()
    }

    #[test]
    fn dates_round_trip() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(fmt_date(parse_date("2027-01-23").unwrap()), "2027-01-23");
        assert_eq!(parse_date("2027-01-23").unwrap() - parse_date("2026-10-18").unwrap() + 1, 98);
        assert_eq!(local_day(0, -60), -1);
    }

    #[test]
    fn plan_has_one_of_each_and_no_repeats() {
        let days = plan_days(98);
        let mut seen = std::collections::HashSet::new();
        for d in &days {
            assert_eq!([d[0].cat, d[1].cat, d[2].cat], ["g", "k", "w"]);
            for q in d {
                assert!(seen.insert(q.text), "repeat: {}", q.text);
            }
        }
    }

    #[tokio::test]
    async fn serves_only_past_days_and_remembers_acks() {
        let app = app_starting_days_ago(5);
        let (s, v) = call(&app, state_req()).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["today"], 6);
        assert_eq!(v["days"].as_array().unwrap().len(), 6);

        assert_eq!(call(&app, ack_req(7, "q0")).await.0, StatusCode::FORBIDDEN);
        assert_eq!(call(&app, ack_req(2, "nope")).await.0, StatusCode::BAD_REQUEST);
        let (s, v) = call(&app, ack_req(2, "grateful")).await;
        assert_eq!(s, StatusCode::OK);
        let first = v["acks"]["grateful"].clone();
        assert!(first.is_string());
        // pressing again keeps the original timestamp
        assert_eq!(call(&app, ack_req(2, "grateful")).await.1["acks"]["grateful"], first);

        let (_, v) = call(&app, state_req()).await;
        assert_eq!(v["days"][1]["acks"]["grateful"], first);
        assert!(v["days"][0]["acks"].as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn healthz_ok() {
        let r = app_starting_days_ago(0).oneshot(Request::get("/healthz").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }
}
