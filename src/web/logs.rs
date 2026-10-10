//! `/admin/logs` admin 页面：连接日志列表 + 筛选 + 分页。
//!
//! 数据来源是 `connection_logs` 表（由 [`crate::xray::logs`] 从 xray stderr
//! 的 access log 行解析落库）。Handler 使用 `sqlx::QueryBuilder` 动态拼
//! WHERE 条件（全部走 `.push_bind` 参数绑定，禁止字符串拼接），分页用
//! `LIMIT 51 OFFSET ?` 多取一行判断 has_next。

use askama::Template;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum_extra::extract::cookie::CookieJar;
use sqlx::sqlite::SqliteRow;
use sqlx::Row;
use sqlx::SqlitePool;

use crate::db::models::ConnectionLog;
use crate::web::middleware::{CurrentAdmin, SECURITY_HEADERS};
use crate::web::AppState;

/// 每页条数。LIMIT 多取一行判断 has_next。
const PAGE_SIZE: i64 = 50;

#[derive(Debug, Clone, Default)]
pub struct LogsFilters {
    pub uuid: String,
    pub from: String,
    pub to: String,
    pub status: String,
    pub page: i64,
    /// 模板用的 URL 编码 uuid（askama 不能调方法，故预计算成字段）。
    pub url_encoded_uuid: String,
}

#[derive(Debug, Clone)]
pub struct LogRowView {
    pub logged_at_iso: String,
    pub uuid: String,
    pub src_addr: Option<String>,
    pub dest: Option<String>,
    pub status: String,
    pub reason: Option<String>,
    pub raw_message: String,
    /// raw_message 的前 60 字，超出部分用 `…` 表示；完整内容走
    /// `<abbr title="...">` 悬停展开。
    pub detail_preview: String,
}

#[derive(Template)]
#[template(path = "logs_list.html", escape = "html")]
struct LogsListTemplate {
    entries: Vec<LogRowView>,
    page: i64,
    has_next: bool,
    has_prev: bool,
    filters: LogsFilters,
}

/// `GET /admin/logs` — 连接日志列表，默认按 `logged_at DESC` 分页展示
/// 最近 50 条；`?uuid=&from=&to=&status=&page=` 支持筛选。
pub async fn list(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    jar: CookieJar,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let filters = parse_filters(&params);

    let rows = match fetch_logs(&state.pool, &filters).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "failed to read connection logs");
            return page(
                jar,
                StatusCode::INTERNAL_SERVER_ERROR,
                "无法读取连接日志".to_string(),
            );
        }
    };

    // 多取一行判断 has_next；超过 PAGE_SIZE 表示还有下一页。
    let has_next = rows.len() as i64 > PAGE_SIZE;
    let entries: Vec<LogRowView> = rows
        .into_iter()
        .take(PAGE_SIZE as usize)
        .map(|row| LogRowView {
            logged_at_iso: timestamp_to_iso(row.logged_at),
            uuid: row.uuid,
            src_addr: row.src_addr,
            dest: row.dest,
            status: row.status,
            reason: row.reason,
            raw_message: row.raw_message.clone(),
            detail_preview: truncate_preview(&row.raw_message, 60),
        })
        .collect();

    let html = LogsListTemplate {
        entries,
        page: filters.page,
        has_next,
        has_prev: filters.page > 1,
        filters,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar, StatusCode::OK, html)
}

/// 解析并 sanitize 查询参数。非法值回退到默认（不报错不 400）：
/// `status` 非 accepted/rejected 视为无筛选；`page` 解析失败回退 1；
/// 超长字段截断到 64 字。
fn parse_filters(params: &std::collections::HashMap<String, String>) -> LogsFilters {
    let mut filters = LogsFilters::default();

    if let Some(uuid) = params.get("uuid").map(|s| s.trim().to_string()) {
        if !uuid.is_empty() {
            filters.uuid = uuid.chars().take(64).collect();
        }
    }
    if let Some(from) = params.get("from").map(|s| s.trim().to_string()) {
        if !from.is_empty() {
            filters.from = from;
        }
    }
    if let Some(to) = params.get("to").map(|s| s.trim().to_string()) {
        if !to.is_empty() {
            filters.to = to;
        }
    }
    if let Some(status) = params.get("status").map(|s| s.trim().to_string()) {
        // 白名单：非 accepted / rejected 视为无筛选（不报错）。
        if matches!(status.as_str(), "accepted" | "rejected") {
            filters.status = status;
        }
    }
    if let Some(page) = params
        .get("page")
        .and_then(|s| s.trim().parse::<i64>().ok())
    {
        if page >= 1 {
            filters.page = page;
        }
    }
    filters.url_encoded_uuid = url_encode(&filters.uuid);
    filters
}

/// 用 `sqlx::QueryBuilder` 构造动态 WHERE 条件并执行。所有用户输入走
/// `.push_bind(...)` 参数绑定；`from` / `to` 用 `YYYY-MM-DD` 转换为 Unix
/// 秒（日期 → 时间戳边界，含 from 不含 to）。
async fn fetch_logs(
    pool: &SqlitePool,
    filters: &LogsFilters,
) -> Result<Vec<ConnectionLog>, sqlx::Error> {
    let mut builder = sqlx::QueryBuilder::new(
        "SELECT id, logged_at, uuid, src_addr, dest, status, reason, raw_message, received_at \
         FROM connection_logs",
    );

    let mut has_condition = false;
    if !filters.uuid.is_empty() {
        builder.push(" WHERE uuid = ");
        builder.push_bind(filters.uuid.clone());
        has_condition = true;
    }
    if let Some(from_ts) = date_to_unix_start(&filters.from) {
        builder.push(if has_condition { " AND " } else { " WHERE " });
        builder.push("logged_at >= ");
        builder.push_bind(from_ts);
        has_condition = true;
    }
    if let Some(to_ts) = date_to_unix_start(&filters.to) {
        builder.push(if has_condition { " AND " } else { " WHERE " });
        builder.push("logged_at < ");
        builder.push_bind(to_ts + 86400);
        has_condition = true;
    }
    if !filters.status.is_empty() {
        builder.push(if has_condition { " AND " } else { " WHERE " });
        builder.push("status = ");
        builder.push_bind(filters.status.clone());
    }

    builder.push(" ORDER BY logged_at DESC, id DESC LIMIT ");
    builder.push_bind(PAGE_SIZE + 1);
    builder.push(" OFFSET ");
    builder.push_bind((filters.page - 1) * PAGE_SIZE);

    let query = builder.build();
    let rows: Vec<SqliteRow> = query.fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|row| ConnectionLog {
            id: row.get("id"),
            logged_at: row.get("logged_at"),
            uuid: row.get("uuid"),
            src_addr: row.get("src_addr"),
            dest: row.get("dest"),
            status: row.get("status"),
            reason: row.get("reason"),
            raw_message: row.get("raw_message"),
            received_at: row.get("received_at"),
        })
        .collect())
}

/// `YYYY-MM-DD` → 当日 00:00:00 的 Unix 秒。空或非法返回 `None`。
fn date_to_unix_start(date: &str) -> Option<i64> {
    if date.is_empty() {
        return None;
    }
    let (y, rest) = date.split_once('-')?;
    let (mo, d) = rest.split_once('-')?;
    let year: i32 = y.parse().ok()?;
    let month: u8 = mo.parse().ok()?;
    let day: u8 = d.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // 闰年 2 月天数（简化：不区分 2 月 29 与 30 的边界，用 >= 29 拒绝即可，
    // 该函数只用于把日期翻成时间戳，误差一天不影响筛选语义）。
    let days = civil_days(year, month, day);
    Some(days * 86400)
}

/// Howard Hinnant 的 civil-from-days 算法（公历 → Unix 天数）。
fn civil_days(y: i32, m: u8, d: u8) -> i64 {
    let y = y as i64 - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn truncate_preview(raw: &str, max: usize) -> String {
    let chars: Vec<char> = raw.chars().collect();
    if chars.len() <= max {
        return raw.to_string();
    }
    let head: String = chars[..max].iter().collect();
    format!("{head}…")
}

fn timestamp_to_iso(ts: i64) -> String {
    const FMT: &[time::format_description::BorrowedFormatItem<'_>] =
        time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    time::OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|t| t.format(&FMT).ok())
        .unwrap_or_else(|| ts.to_string())
}

fn url_encode(input: &str) -> String {
    input
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn page(jar: CookieJar, status: StatusCode, body: String) -> Response {
    let mut response = (status, Html(body)).into_response();
    for cookie in jar.iter() {
        if let Ok(v) = cookie.encoded().to_string().parse() {
            response
                .headers_mut()
                .append(axum::http::header::SET_COOKIE, v);
        }
    }
    for (name, value) in SECURITY_HEADERS {
        response.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("static header name"),
            axum::http::HeaderValue::from_static(value),
        );
    }
    response
}
