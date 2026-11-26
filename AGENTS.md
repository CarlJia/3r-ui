# 3R-UI 项目开发与提交规范

## CI / 代码格式与质量检查 (Pre-commit / Pre-push 必须执行)

在提交与推送代码前，**必须**在本地依次执行并确保以下所有检查通过：

1. **Rust 代码格式化检查：**
   ```bash
   cargo fmt --check
   ```
   若未通过，必须先执行 `cargo fmt` 自动修复所有格式问题。

2. **Clippy 静态检查：**
   ```bash
   cargo clippy --all-targets -- -D warnings
   ```
   必须确保 0 warning、0 error。

3. **测试套件验证：**
   ```bash
   cargo test
   ```
   所有单元测试与集成测试必须全部通过。

## 目录与忽略文件规范

- IDE 及本地运行生成的临时文件（如 `.idea/`、`.vscode/`、`.run/`、`*.iml`、`.codegraph/`）一律保持在 `.gitignore` 中，严禁提交到 git 仓库。
