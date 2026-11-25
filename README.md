# 3R-UI

高性能、轻量级、自托管的代理核心可视化管理面板 (基于 Rust + Axum + SQLite 构建)。支持 VLESS (REALITY / XHTTP)、Hysteria 2、Mieru 入站协议，并内置 Pingora 反向代理与自动申请 TLS 证书。

---

## 快速安装与部署 (Quick Start)

### 方式一：在远程 VPS 上一键在线安装（推荐）

直接在您的 Debian 或 Ubuntu VPS（支持 x86_64 和 ARM64）终端中执行以下命令：

```bash
bash <(curl -fsSL https://raw.githubusercontent.com/CarlJia/3r-ui/main/packaging/install.sh)
```

脚本将自动完成：
1. 识别系统发行版与硬件架构（x86_64 / aarch64）。
2. 从 GitHub Releases 下载适配的预编译二进制并安装至 `/usr/local/bin/3r-ui`。
3. 配置系统服务 `/etc/systemd/system/3r-ui.service` 与环境配置 `/etc/3r-ui/3r-ui.env`。
4. 注册开机自启并启动 3R-UI 服务。
5. 打印初始管理员账号与访问密码。

---

### 方式二：本地一键远程批量部署 VPS

如果您在本地机器拥有代码，希望通过 SSH 直接远程配置并推送部署一台或多台 VPS：

1. **准备配置文件**：
   ```bash
   cp scripts/deploy.conf.example scripts/deploy.conf
   ```
2. **编辑 `scripts/deploy.conf`**，填写 VPS 的 IP、SSH 端口、密钥及面板运行参数。
3. **执行一键部署**：
   ```bash
   ./scripts/deploy.sh
   ```

---

## 常用管理命令

```bash
# 查看服务运行状态
systemctl status 3r-ui

# 查看实时运行日志
journalctl -u 3r-ui -f

# 查看初始管理员账号与密码
journalctl -u 3r-ui -n 50 --no-pager | grep -A 4 'admin password'

# 重启服务
systemctl restart 3r-ui
```

---

## 声明与免责条款 (Disclaimer)

### 1. 软件用途与定位
- 本项目（3R-UI）仅作为一个通用网络反向代理与进程管理的可视化运维工具，旨在帮助系统运维人员和开发者便捷地管理自身 VPS 主机上的网络服务、反向代理与日志审计。
- 本项目本身**不提供、不分发、不托管**任何网络节点、翻墙服务或任何侵犯知识产权的内容。

### 2. 免责声明 (Disclaimer of Liability)
- **遵守当地法律法规**：使用者在下载、编译、安装和运行本项目时，必须严格遵守所在国家和地区的法律、法规及相关政策规定。严禁将本项目用于任何违反法律法规、侵犯国家安全、侵犯他人隐私或从事任何非法网络活动的用途。
- **使用者全权负责**：对于任何个人或组织因不正当使用、非法传播或将本项目用于任何违法违规行为所造成的一切直接或间接法律责任、安全风险、经济损失或行政处罚，均由使用者自行完全承担，本项目开发者与贡献者概不承担任何连带责任。
- **无担保声明**：本项目遵循开源协议按“现状”（AS IS）提供，开发者不提供任何明示或暗示的担保（包括但不限于适销性、特定用途适用性或无错误运行等）。因使用本项目导致的任何软硬件故障、数据丢失或业务中断，开发者不承担任何赔偿责任。

---

## 开源许可证 (License)

本项目遵循 [GNU General Public License v3.0 or later (GPL-3.0-or-later)](./LICENSE) 协议开源。
你可以自由地使用、修改和分发本项目代码，但任何衍生项目或修改版本的源码均须在 GPL-3.0 协议下保持开源。
