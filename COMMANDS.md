# 常用操作命令备忘

记录 web dashboard / 开机自启动相关的实操命令，方便日后查阅。

## 编译

```powershell
cargo build --release
```

产物在 `target\release\clauth.exe`。开发验证阶段图快也可以用 `cargo run -- <子命令>`（等价于先 `cargo build`，产物在 `target\debug\clauth.exe`）。

## 手动运行 dashboard / daemon

```powershell
clauth daemon
```

会常驻在当前终端（关闭终端进程就没了），同时会：
- 监听 `http://127.0.0.1:47893/`（网页 dashboard）
- 把状态写到 `~/.clauth/status.json`

查询有没有 daemon 在跑（不会启动任何东西，纯查询）：

```powershell
clauth daemon --status
```

输出类似 `running (pid <n>, feed fresh|stale)`；没有daemon则退出码为 1、无输出。

## 开机自动运行（Windows 任务计划程序）

```powershell
# 安装：注册一个"登录时运行"的任务，无可见窗口
clauth autostart install -y

# 如果你的网络访问 Claude 需要代理，显式指定（不依赖当前终端有没有设代理）：
clauth autostart install --proxy http://127.0.0.1:7890 -y

# 卸载
clauth autostart uninstall -y

# 查询是否已注册
clauth autostart status
```

**代理说明**：
- `--proxy <url>` 会把这个值写死进生成的隐藏启动脚本 `~/.clauth/autostart_launch.vbs` 里，只作用于这个脚本启动出来的那一个 `clauth.exe daemon` 进程，不会影响系统里其他任何程序，也不需要设置系统级/用户级环境变量（不用 `setx`）。
- 不传 `--proxy` 时，会自动读取你执行 `install` 那一刻、那个终端窗口里的 `HTTP_PROXY`/`HTTPS_PROXY`。
- 代理值是**安装那一刻的静态快照**，不是每次开机都重新读取——以后如果代理地址变了，需要重新执行一次 `install`（会用 `/F` 覆盖旧配置）。
- `install` 本身**只是注册配置，不会立刻启动 daemon**。

**这套代理配置只在"通过任务计划程序触发"时生效**（包括真正开机登录、或者手动 `schtasks /Run` 触发）；如果你绕开任务计划程序、自己直接手动敲 `clauth daemon`，走的是这个终端自己的环境变量，跟 autostart 那套代理机制无关，还是老规矩需要这个终端自己先设好代理。

## 立即触发一次已注册的任务（不用等下次登录）

```powershell
schtasks /Run /TN clauth-daemon
```

跑起来后打开任务管理器"详细信息"页可以看到一个无窗口的 `clauth.exe` 在跑；任务计划程序自己的"状态"栏很快会变回"准备就绪"——这是正常的，因为它启动出来的脚本任务本身瞬间执行完（脚本用 `Run(cmd, 0, False)` 是"启动完不等待"），daemon 进程早就独立于任务计划程序在后台跑了，不代表 daemon 已经退出。

## 清理 / 排障

```powershell
# 杀掉所有 clauth.exe 进程（连带手动开的那些）
taskkill /IM clauth.exe /F

# 确认没有残留进程
Get-Process clauth -ErrorAction SilentlyContinue

# 看谁占着 dashboard 端口
Get-NetTCPConnection -LocalPort 47893 | Select-Object LocalPort, OwningProcess, State
```

如果同时开了多个 `clauth.exe`（比如手动跑一个 + 任务计划程序又触发了一个），建议先 `taskkill` 全部清掉，再只保留一个入口重新启动，避免多进程互相抢用量抓取锁导致数据不刷新。
