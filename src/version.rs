//! 3R-UI 应用版本定义与工具函数
//!
//! 在编译期由 `build.rs` 自动提取 git 最新 tag 或从 `APP_VERSION` 环境变量注入。

/// 3R-UI 面板版本（格式统一带 'v' 前缀，例如 "v0.0.4"）
pub const APP_VERSION: &str = env!("APP_VERSION");

/// 获取剥离 'v' 前缀的语义化版本号（例如 "0.0.4"）
#[inline]
pub fn app_version_stripped() -> &'static str {
    if let Some(stripped) = APP_VERSION.strip_prefix('v') {
        stripped
    } else if let Some(stripped) = APP_VERSION.strip_prefix('V') {
        stripped
    } else {
        APP_VERSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_format() {
        assert!(!APP_VERSION.is_empty(), "APP_VERSION should not be empty");
        assert!(
            APP_VERSION.starts_with('v') || APP_VERSION.starts_with('V'),
            "APP_VERSION should start with v/V prefix: {}",
            APP_VERSION
        );
        let stripped = app_version_stripped();
        assert!(
            !stripped.starts_with('v') && !stripped.starts_with('V'),
            "app_version_stripped should not start with v/V: {}",
            stripped
        );
    }
}
