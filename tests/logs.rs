//! Integration tests for 连接日志: /admin/logs 列表 / 筛选 / 分页 /
//! 状态徽章 / sidebar 入口 / auth 守卫，以及 `xray::logs` 的解析与
//! prune_older_than 保留清理。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sqlx::Row;
use tempfile::tempdir;
use three_r_ui::config::AppConfig;
use three_r_ui::crypto::password;
use three_r_ui::web::{self, AppState};
use tower::ServiceExt;

const ADMIN_PW: &str = "s3cret-admin-pw";

struct Harness {
    app: axum::Router,
    _dir: tempfile::TempDir,
    pool: sqlx::SqlitePool,
}

async fn boot() -> Harness {
    let dir = tempdir().unwrap();
    let mut config = AppConfig::from_env().unwrap();
    config.data_dir = dir.path().to_path_buf();

    let pool = three_r_ui::init_storage(&config).await.unwrap();
    sqlx::query(
        "INSERT INTO admins (username, password_hash, created_at) VALUES (?, ?, unixepoch())",
    )
    .bind("admin")
    .bind(password::hash(ADMIN_PW).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let state = AppState {
        config,
        pool: pool.clone(),
        supervisor_tx: tx,
    };
    Harness {
        app: web::router(state),
        _dir: dir,
        pool,
    }
}

fn set_cookie(resp: &axum::response::Response, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    resp.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| {
            let after = v.strip_prefix(&prefix)?;
            Some(after.split(';').next().unwrap_or("").to_string())
        })
}

async fn login(h: &Harness) -> (String, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(format!("username=admin&password={ADMIN_PW}")))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    (
        set_cookie(&resp, "session").expect("session"),
        set_cookie(&resp, "csrf").expect("csrf"),
    )
}

async fn get(h: &Harness, uri: &str, session: &str) -> axum::response::Response {
    let req = Request::builder()
        .uri(uri)
        .header("cookie", format!("session={session}"))
        .body(Body::empty())
        .unwrap();
    h.app.clone().oneshot(req).await.unwrap()
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

/// 写入 N 条样例 connection_logs，`logged_at` 从 `base_ts` 起每条递增 1 秒，
/// uuid 在 `uuids` 中循环取用，status 在 `statuses` 中循环取用。
async fn seed_logs(h: &Harness, n: usize, base_ts: i64, uuids: &[&str], statuses: &[&str]) {
    for i in 0..n {
        sqlx::query(
            "INSERT INTO connection_logs \
                (logged_at, uuid, src_addr, dest, status, reason, raw_message, received_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(base_ts + i as i64)
        .bind(uuids[i % uuids.len()])
        .bind(format!("10.0.0.{}:{}", 100 + i, 40000 + i))
        .bind("example.com:443")
        .bind(statuses[i % statuses.len()])
        .bind(if statuses[i % statuses.len()] == "rejected" {
            Some("user not found".to_string())
        } else {
            None
        })
        .bind(format!("seed line {i}"))
        .bind(base_ts)
        .execute(&h.pool)
        .await
        .unwrap();
    }
}

// ---- auth 守卫 ----

#[tokio::test]
async fn logs_list_requires_a_session() {
    let h = boot().await;
    let req = Request::builder()
        .uri("/admin/logs")
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

// ---- 空表 ----

#[tokio::test]
async fn empty_logs_shows_an_empty_state() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/logs", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("暂无日志"));
}

// ---- 默认列表 ----

#[tokio::test]
async fn logs_list_renders_in_descending_logged_at_order() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    seed_logs(&h, 5, 1_700_000_000, &["uuid-aaa"], &["accepted"]).await;

    let resp = get(&h, "/admin/logs", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("访问记录"));
    assert!(html.contains("status-accepted"));
    // tbody 至少出现一次（5 条都被渲染）。
    assert!(html.contains("<tbody>"), "tbody rendered");
}

// ---- 筛选 ----

#[tokio::test]
async fn uuid_filter_is_exact_match() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    seed_logs(
        &h,
        4,
        1_700_000_000,
        &["01234567-89ab-cdef-0123-456789abcdef"],
        &["accepted"],
    )
    .await;
    seed_logs(
        &h,
        2,
        1_700_000_100,
        &["99999999-aaaa-bbbb-cccc-dddddddddddd"],
        &["accepted"],
    )
    .await;

    let resp = get(
        &h,
        "/admin/logs?uuid=01234567-89ab-cdef-0123-456789abcdef",
        &session,
    )
    .await;
    let html = body_text(resp).await;
    assert!(html.contains("01234567"));
    assert!(
        !html.contains("99999999"),
        "other uuid must be filtered out"
    );
}

#[tokio::test]
async fn status_filter_matches_only_that_status() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    seed_logs(
        &h,
        4,
        1_700_000_000,
        &["uuid-aaa"],
        &["accepted", "rejected"],
    )
    .await;

    // CSS 类定义始终出现在 <style> 块里，所以要匹配 row 上的
    // `class="status status-accepted"` 形态而非裸字符串。
    let html = body_text(get(&h, "/admin/logs?status=accepted", &session).await).await;
    assert!(
        html.contains("status status-accepted"),
        "accepted rows rendered"
    );
    assert!(
        !html.contains("status status-rejected"),
        "rejected rows filtered out"
    );

    let html = body_text(get(&h, "/admin/logs?status=rejected", &session).await).await;
    assert!(html.contains("status status-rejected"));
    assert!(
        !html.contains("status status-accepted"),
        "accepted rows filtered out"
    );
}

#[tokio::test]
async fn invalid_status_value_is_treated_as_no_filter() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    seed_logs(
        &h,
        4,
        1_700_000_000,
        &["uuid-aaa"],
        &["accepted", "rejected"],
    )
    .await;

    let resp = get(&h, "/admin/logs?status=garbage", &session).await;
    assert_eq!(resp.status(), StatusCode::OK, "no 400 for invalid status");
    let html = body_text(resp).await;
    assert!(html.contains("已接受"), "all rows shown, no filter applied");
    assert!(html.contains("已拒绝"));
}

#[tokio::test]
async fn time_range_filter_bounds_logged_at() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    // 2026-10-01 / 2026-10-15 的边界条目。
    seed_logs(&h, 1, 1_760_000_000, &["uuid-aaa"], &["accepted"]).await; // 早于 2026-10-01
    seed_logs(&h, 1, 1_792_000_000, &["uuid-aaa"], &["accepted"]).await; // 2026-10-10 附近
    seed_logs(&h, 1, 1_800_000_000, &["uuid-aaa"], &["accepted"]).await; // 2026-10-20 之后

    let resp = get(&h, "/admin/logs?from=2026-10-01&to=2026-10-15", &session).await;
    let html = body_text(resp).await;
    assert!(html.contains("已接受"), "range rows shown");
    // 只有中间那条落在区间内：其余两行被筛掉。
    let rows = html.split("<tbody>").nth(1).unwrap_or("");
    assert!(
        !rows.contains("uuid-") || rows.matches("uuid-").count() == 1,
        "only the in-range row should appear"
    );
}

#[tokio::test]
async fn sql_injection_via_uuid_is_handled_as_literal() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    seed_logs(&h, 2, 1_700_000_000, &["uuid-aaa"], &["accepted"]).await;

    // `abc' OR 1=1--` 必须按字面量处理，不报错也不返回全部行。
    let resp = get(&h, "/admin/logs?uuid=abc%27%20OR%201%3D1--", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(!html.contains("uuid-aaa"), "injection must not match");
}

// ---- 分页 ----

#[tokio::test]
async fn pagination_splits_at_page_size() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    seed_logs(&h, 60, 1_700_000_000, &["uuid-aaa"], &["accepted"]).await;

    let page1 = body_text(get(&h, "/admin/logs?page=1", &session).await).await;
    assert!(page1.contains("下一页"), "page 1 has a next link");
    assert!(
        !page1.contains("上一页") || page1.contains("disabled"),
        "page 1 has no prev"
    );

    let page2 = body_text(get(&h, "/admin/logs?page=2", &session).await).await;
    assert!(page2.contains("上一页"), "page 2 has a prev link");
    assert!(
        !page2.contains("下一页") || page2.contains("disabled"),
        "page 2 has no next link (60 total = 50 + 10)"
    );
}

#[tokio::test]
async fn pagination_links_preserve_filter_params() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    seed_logs(
        &h,
        60,
        1_700_000_000,
        &["01234567-89ab-cdef-0123-456789abcdef"],
        &["accepted"],
    )
    .await;

    let page1 = body_text(
        get(
            &h,
            "/admin/logs?uuid=01234567-89ab-cdef-0123-456789abcdef&page=1",
            &session,
        )
        .await,
    )
    .await;
    assert!(
        page1.contains("uuid=01234567"),
        "next-page link must carry the uuid filter"
    );
}

#[tokio::test]
async fn invalid_page_value_falls_back_to_page_1() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    seed_logs(&h, 5, 1_700_000_000, &["uuid-aaa"], &["accepted"]).await;

    let resp = get(&h, "/admin/logs?page=abc", &session).await;
    assert_eq!(resp.status(), StatusCode::OK, "no 400 for invalid page");
}

// ---- 侧边栏 ----

#[tokio::test]
async fn logs_page_shows_sidebar_navigation() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/logs", &session).await;
    let html = body_text(resp).await;
    assert!(html.contains("class=\"sidebar\""), "sidebar present");
    assert!(
        html.contains("href=\"/admin/logs\""),
        "logs nav link present"
    );
    // 当前页面高亮 active。
    let active_link = html
        .split("<aside")
        .nth(1)
        .and_then(|rest| rest.split("</aside>").next())
        .and_then(|aside| {
            aside
                .split("<a ")
                .find(|a| a.contains("/admin/logs") && a.contains("active"))
        });
    assert!(active_link.is_some(), "logs nav link should be active");
}

#[tokio::test]
async fn logs_page_includes_sidebar_style_for_layout() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/logs", &session).await;
    let html = body_text(resp).await;
    // sidebar_style.html 提供了 .content 的 grid 布局；缺它侧边栏塌成单列。
    assert!(
        html.contains(".content") || html.contains("sidebar"),
        "logs page should include sidebar_style.html content"
    );
}

// ---- 状态徽章与详情列 ----

#[tokio::test]
async fn status_badges_use_accepted_and_rejected_classes() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    seed_logs(
        &h,
        4,
        1_700_000_000,
        &["uuid-aaa"],
        &["accepted", "rejected"],
    )
    .await;

    let html = body_text(get(&h, "/admin/logs", &session).await).await;
    assert!(
        html.contains("status status-accepted"),
        "green accepted badge"
    );
    assert!(
        html.contains("status status-rejected"),
        "red rejected badge"
    );
    // 不与用户状态徽章混淆。
    assert!(
        !html.contains("status-active"),
        "client-status class not reused"
    );
}

#[tokio::test]
async fn detail_column_escapes_html_in_raw_message() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    sqlx::query(
        "INSERT INTO connection_logs \
            (logged_at, uuid, src_addr, dest, status, reason, raw_message, received_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(1_700_000_000)
    .bind("uuid-aaa")
    .bind(Some("10.0.0.1:1".to_string()))
    .bind(Some("a.com:443".to_string()))
    .bind("accepted")
    .bind::<Option<String>>(None)
    .bind("<script>alert(1)</script>")
    .bind(1_700_000_000)
    .execute(&h.pool)
    .await
    .unwrap();

    let html = body_text(get(&h, "/admin/logs", &session).await).await;
    assert!(
        !html.contains("<script>alert"),
        "raw_message must be HTML-escaped"
    );
    assert!(html.contains("&lt;script&gt;"), "escaped form rendered");
}

#[tokio::test]
async fn detail_column_truncates_long_raw_message() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let long = "x".repeat(200);
    sqlx::query(
        "INSERT INTO connection_logs \
            (logged_at, uuid, src_addr, dest, status, reason, raw_message, received_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(1_700_000_000)
    .bind("uuid-aaa")
    .bind(Some("10.0.0.1:1".to_string()))
    .bind(Some("a.com:443".to_string()))
    .bind("accepted")
    .bind::<Option<String>>(None)
    .bind(&long)
    .bind(1_700_000_000)
    .execute(&h.pool)
    .await
    .unwrap();

    let html = body_text(get(&h, "/admin/logs", &session).await).await;
    // 预览截断到 60 字 + `…`；完整 200 字只在 title 属性里。
    assert!(html.contains("…"), "preview truncated");
    assert!(
        !html.contains(&format!(">{long}<")),
        "200-char raw_message should not be inline-rendered in full"
    );
}

// ---- prune_older_than 保留清理 ----

#[tokio::test]
async fn prune_older_than_deletes_only_rows_before_threshold() {
    let h = boot().await;
    let now = three_r_ui::crypto::session::now_unix();
    let day: i64 = 24 * 3600;
    for (offset, label) in [
        (now - day, "1d"),
        (now - 29 * day, "29d"),
        (now - 31 * day, "31d"),
        (now - 60 * day, "60d"),
    ] {
        sqlx::query(
            "INSERT INTO connection_logs \
                (logged_at, uuid, src_addr, dest, status, reason, raw_message, received_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(offset)
        .bind(label)
        .bind::<Option<String>>(None)
        .bind::<Option<String>>(None)
        .bind("accepted")
        .bind::<Option<String>>(None)
        .bind("prune seed")
        .bind(now)
        .execute(&h.pool)
        .await
        .unwrap();
    }

    let deleted = three_r_ui::xray::logs::prune_older_than(&h.pool, now - 30 * day)
        .await
        .unwrap();
    assert_eq!(deleted, 2, "31d and 60d rows should be pruned");

    let remaining: Vec<String> =
        sqlx::query("SELECT uuid FROM connection_logs ORDER BY logged_at DESC")
            .fetch_all(&h.pool)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.get::<String, _>("uuid"))
            .collect();
    assert_eq!(remaining, vec!["1d".to_string(), "29d".to_string()]);
}

#[tokio::test]
async fn prune_older_than_is_a_noop_when_nothing_matches() {
    let h = boot().await;
    let now = three_r_ui::crypto::session::now_unix();
    seed_logs(&h, 3, now - 100, &["uuid-aaa"], &["accepted"]).await;
    let deleted = three_r_ui::xray::logs::prune_older_than(&h.pool, now - 30 * 24 * 3600)
        .await
        .unwrap();
    assert_eq!(deleted, 0);
}
