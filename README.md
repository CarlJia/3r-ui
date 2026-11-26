# 3R-UI

高性能、轻量级、自托管的代理核心可视化管理面板 (基于 Rust + Axum + SQLite 构建)。  
原生支持 **VLESS (REALITY / XHTTP)**、**Hysteria 2**、**Mieru** 协议，并内置 **Pingora** 现代反向代理与 Cloudflare DNS-01 自动化证书管理。

---

## 目录
- [功能特性](#功能特性)
- [快速安装与部署](#快速安装与部署)
  - [方式一：VPS 终端一键安装 (推荐)](#方式一vps-终端一键安装-推荐)
  - [方式二：本地一键远程批量部署 VPS](#方式二本地一键远程批量部署-vps)
- [版本更新与升级](#版本更新与升级)
  - [方式一：一键自动无损升级 (推荐)](#方式一一键自动无损升级-推荐)
  - [方式二：指定本地二进制文件升级](#方式二指定本地二进制文件升级)
  - [方式三：本地脚本远程批量升级](#方式三本地脚本远程批量升级)
- [初次登录与访问配置](#初次登录与访问配置)
  - [1. 查看初始管理员密码](#1-查看初始管理员密码)
  - [2. 开放公网浏览器访问 (修改监听地址)](#2-开放公网浏览器访问-修改监听地址)
  - [3. 配置 PUBLIC_HOST (节点与订阅域名)](#3-配置-public_host-节点与订阅域名)
- [常用管理命令](#常用管理命令)
- [一键完全卸载](#一键完全卸载)
- [免责声明与许可证](#免责声明与许可证)

---

## 功能特性

- **现代协议支持**：
  - **VLESS**：支持标准 TCP、XHTTP 传输通道及 REALITY 白名单伪装。
  - **Hysteria 2**：高性能 QUIC/UDP 协议，多用户认证统一管理。
  - **Mieru**：原生集成防 GFW 主动探测协议，支持多端口/端口段（Port Range）及传输协议切换。
- **极低资源开销**：全异步 Rust 编写，静态单二进制，仅占用 10-20MB 左右常驻内存。
- **系统监控与诊断**：控制台提供高精度 CPU、内存、负载、网络 in/out 实时仪表盘及流量统计。
- **内置反向代理**：内置 Cloudflare Pingora 反向代理，支持自动续签 TLS 证书。

---

## 快速安装与部署

### 方式一：VPS 终端一键安装 (推荐)

在您的 **Debian (11/12)** 或 **Ubuntu (20.04/22.04/24.04)** VPS 终端（支持 x86_64 及 ARM64 架构），以 `root` 或具备 `sudo` 权限的用户直接执行：

```bash
bash <(curl -fsSL https://raw.githubusercontent.com/CarlJia/3r-ui/main/packaging/install.sh)
```

> **提示（指定本地上传的二进制文件）**：  
> 若您已将 GitHub Actions 构建生成的制品（例如 `3r-ui-x86_64-unknown-linux-musl`）上传至 VPS（如 `/root/3r-ui-x86_64-unknown-linux-musl`），可通过参数直接指定安装：
> ```bash
> # 方式 A：通过参数传递 (推荐)
> bash <(curl -fsSL https://raw.githubusercontent.com/CarlJia/3r-ui/main/packaging/install.sh) -s /root/3r-ui-x86_64-unknown-linux-musl
> 
> # 方式 B：通过环境变量指定
> LOCAL_BIN=/root/3r-ui-x86_64-unknown-linux-musl bash <(curl -fsSL https://raw.githubusercontent.com/CarlJia/3r-ui/main/packaging/install.sh)
> ```

---

### 方式二：本地一键远程批量部署 VPS

如果您在本地电脑拥有代码，希望通过 SSH 直接远程配置并推送部署一台或多台 VPS：

1. **准备配置文件**：
   ```bash
   cp scripts/deploy.conf.example scripts/deploy.conf
   ```
2. **编辑 `scripts/deploy.conf`**，填写 VPS 的 IP、SSH 端口、用户名、密钥或密码。
3. **执行一键部署**：
   ```bash
   ./scripts/deploy.sh
   ```

---

## 版本更新与升级

### 方式一：一键自动无损升级 (推荐)

在 VPS 终端直接再次运行一键安装脚本，系统会自动获取 GitHub 最新 Release 发布的对应架构二进制文件并完成热替换。升级过程**自动保留**现有的环境配置（`/etc/3r-ui/3r-ui.env`）、数据库及所有节点与用户数据（`/var/lib/3r-ui/3r-ui.db`）和 TLS 证书，并自动执行最新的数据库版本迁移：

```bash
bash <(curl -fsSL https://raw.githubusercontent.com/CarlJia/3r-ui/main/packaging/install.sh)
```

### 方式二：指定本地二进制文件升级

若您自行编译了最新二进制文件或下载了指定架构的构建包，上传至 VPS（例如 `/root/3r-ui-x86_64-unknown-linux-musl`）后可直接指定路径升级：

```bash
bash <(curl -fsSL https://raw.githubusercontent.com/CarlJia/3r-ui/main/packaging/install.sh) -s /root/3r-ui-x86_64-unknown-linux-musl
```

### 方式三：本地脚本远程批量升级

使用本地 `scripts/deploy.sh` 脚本可一键将本地编译好的最新 release 二进制分发并自动重启多台远程 VPS：

```bash
./scripts/deploy.sh
```

---

## 初次登录与访问配置

### 1. 查看初始管理员密码

安装完成后，系统会自动生成随机管理员账户与密码。在 VPS 执行以下命令查看：

```bash
journalctl -u 3r-ui -n 50 --no-pager | grep -A 4 'admin password'
```

### 2. 开放公网浏览器访问 (修改监听地址)

面板默认监听回环地址 `127.0.0.1:2053`，若要直接通过浏览器公网访问：

```bash
# 1. 修改监听地址为 0.0.0.0:2053
sed -i 's/BIND_ADDR=127.0.0.1:2053/BIND_ADDR=0.0.0.0:2053/' /etc/3r-ui/3r-ui.env

# 2. 重启服务生效
systemctl restart 3r-ui

# 3. 放行防火墙端口 (若启用了 ufw)
ufw allow 2053/tcp
```
完成之后，直接在浏览器中打开：`http://<您的VPS公网IP>:2053` 即可登录。

### 3. 配置 PUBLIC_HOST (节点与订阅域名)

为了让客户端生成的分享链接和订阅 URL 能够正常解析连接，请将您的 VPS 公网 IP 或解析域名写入环境配置：

```bash
# 自动检测 VPS 公网 IP 并写入配置
IP=$(curl -fsSL4 https://api.ipify.org || hostname -I | awk '{print $1}')
sed -i "s/^PUBLIC_HOST=.*/PUBLIC_HOST=$IP/" /etc/3r-ui/3r-ui.env
systemctl restart 3r-ui

# 或者手动指定域名：
# sed -i 's/^PUBLIC_HOST=.*/PUBLIC_HOST=vpn.yourdomain.com/' /etc/3r-ui/3r-ui.env
# systemctl restart 3r-ui
```

---

## 常用管理命令

```bash
# 查看 3R-UI 面板运行状态
systemctl status 3r-ui

# 查看实时运行日志
journalctl -u 3r-ui -f

# 启动 / 停止 / 重启服务
systemctl start 3r-ui
systemctl stop 3r-ui
systemctl restart 3r-ui

# 查看网络端口监听状态
ss -tulpn | grep -E "xray|3r-ui|mita"
```

---

## 一键完全卸载

如果您不再需要 3R-UI，可以在 VPS 上直接执行一键卸载：

```bash
# 方式 1：直接运行专属卸载脚本
bash <(curl -fsSL https://raw.githubusercontent.com/CarlJia/3r-ui/main/packaging/uninstall.sh)

# 方式 2：或者通过 install 脚本传递 uninstall 参数
bash <(curl -fsSL https://raw.githubusercontent.com/CarlJia/3r-ui/main/packaging/install.sh) uninstall
```

脚本将自动停止服务、清理后台进程、删除系统单元与可执行文件，并交互式询问是否清除数据库与日志。  
*(若需静默全自动卸载并彻底清空所有配置数据，可加上 `-y --purge` 参数)*：
```bash
bash <(curl -fsSL https://raw.githubusercontent.com/CarlJia/3r-ui/main/packaging/uninstall.sh) -y --purge
```

---

## 免责声明与许可证

### 声明与免责条款 (Disclaimer)

1. **软件用途与定位**：本项目仅作为一个通用网络反向代理与进程管理的可视化运维工具，旨在帮助系统运维人员和开发者便捷地管理自身 VPS 主机上的网络服务、反向代理与日志审计。本项目本身**不提供、不分发、不托管**任何网络节点、翻墙服务或任何侵犯知识产权的内容。
2. **免责声明 (Disclaimer of Liability)**：
   - 使用者在下载、编译、安装和运行本项目时，必须严格遵守所在国家和地区的法律、法规及相关政策规定。严禁将本项目用于任何违反法律法规、侵犯国家安全、侵犯他人隐私或从事任何非法网络活动的用途。
   - 对于任何个人或组织因不正当使用、非法传播或将本项目用于任何违法违规行为所造成的一切直接或间接法律责任、安全风险、经济损失或行政处罚，均由使用者自行完全承担，本项目开发者与贡献者概不承担任何连带责任。
   - 本项目遵循开源协议按“现状”（AS IS）提供，开发者不提供任何明示或暗示的担保。因使用本项目导致的任何软硬件故障、数据丢失或业务中断，开发者不承担任何赔偿责任。

### 开源许可证 (License)

本项目遵循 [GNU General Public License v3.0 or later (GPL-3.0-or-later)](./LICENSE) 协议开源。
