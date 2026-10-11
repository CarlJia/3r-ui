//! `/admin/logs` admin 页面：连接日志列表 + 筛选 + 分页。
//!
//! 数据来源是 `connection_logs` 表（由 [`crate::xray::logs`] 从 xray stderr
//! 的 access log 行解析落库）。Handler 使用 `sqlx::QueryBuilder` 动态拼
//! WHERE 条件（全部走 `.push_bind` 参数绑定，禁止字符串拼接），分页用
//! `LIMIT 51 OFFSET ?` 多取一行判断 has_next。

use askama::Template;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum_extra::extract::cookie::CookieJar;
use sqlx::SqlitePool;

use crate::db::models::ConnectionLog;
use crate::web::middleware::{render_page, url_encode, CurrentAdmin};
use crate::web::AppState;

/// 每页条数。LIMIT 多取一行判断 has_next。
const PAGE_SIZE: i64 = 50;

/// 页号上限，保证 `(page - 1) * PAGE_SIZE` 不溢出 i64。
const MAX_PAGE: i64 = i64::MAX / PAGE_SIZE;

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
    /// 前 8 字符预览（多字节安全：不足或含多字节字符时退回完整值）。
    pub uuid_preview: String,
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
            return render_page(
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
        .map(|row| {
            // 先算 preview 再 move raw_message，省一次 String clone。
            let detail_preview = truncate_preview(&row.raw_message, 60);
            LogRowView {
                logged_at_iso: timestamp_to_iso(row.logged_at),
                uuid_preview: row.uuid.get(..8).unwrap_or(&row.uuid).to_string(),
                src_addr: row.src_addr,
                dest: row.dest,
                status: row.status,
                reason: row.reason,
                raw_message: row.raw_message,
                detail_preview,
            }
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
    render_page(jar, StatusCode::OK, html)
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
        // 上限防溢出：fetch_logs 会算 (page-1)*PAGE_SIZE，无上限时
        // page 接近 i64::MAX 会 debug panic / release 回绕成负 OFFSET。
        if page >= 1 {
            filters.page = page.min(MAX_PAGE);
        }
    }
    filters.url_encoded_uuid = url_encode(&filters.uuid);
    filters
}

/// 用 `sqlx::QueryBuilder` 构造动态 WHERE 条件并执行。所有用户输入走
/// `.push_bind(...)` 参数绑定；`from` / `to` 按 `YYYY-MM-DD` 转换为该日
/// 00:00:00 的 Unix 秒，筛选区间为 `[from 0点, to+1天 0点)`（即含 to 当天
/// 全天，与日期选择器的直觉一致）。
async fn fetch_logs(
    pool: &SqlitePool,
    filters: &LogsFilters,
) -> Result<Vec<ConnectionLog>, sqlx::Error> {
    let mut builder = sqlx::QueryBuilder::new(
        "SELECT id, logged_at, uuid, src_addr, dest, status, reason, raw_message, received_at \
         FROM connection_logs WHERE 1=1",
    );

    if !filters.uuid.is_empty() {
        builder.push(" AND uuid = ").push_bind(filters.uuid.clone());
    }
    if let Some(from_ts) = date_to_unix_start(&filters.from) {
        builder.push(" AND logged_at >= ").push_bind(from_ts);
    }
    if let Some(to_ts) = date_to_unix_start(&filters.to) {
        builder.push(" AND logged_at < ").push_bind(to_ts + 86400);
    }
    if !filters.status.is_empty() {
        builder
            .push(" AND status = ")
            .push_bind(filters.status.clone());
    }

    builder
        .push(" ORDER BY logged_at DESC, id DESC LIMIT ")
        .push_bind(PAGE_SIZE + 1)
        .push(" OFFSET ")
        .push_bind((filters.page - 1) * PAGE_SIZE);

    builder
        .build_query_as::<ConnectionLog>()
        .fetch_all(pool)
        .await
}

/// `YYYY-MM-DD` → 当日 00:00:00 的 Unix 秒。空或非法返回 `None`。
/// 复用 `crypto::session::parse_iso_date`（含完整日历校验，拒绝
/// 2026-02-30 之类非法日期）。
fn date_to_unix_start(date: &str) -> Option<i64> {
    let date = crate::crypto::session::parse_iso_date(date)?;
    Some(
        time::PrimitiveDateTime::new(date, time::Time::MIDNIGHT)
            .assume_utc()
            .unix_timestamp(),
    )
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
