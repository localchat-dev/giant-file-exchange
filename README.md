# 文件交换助手

面向 Windows 的公司文件交换桌面客户端。应用使用个人令牌调用文件交换 HTTP 接口，支持资源管理器右键上传、顺序上传进度队列，以及通过全局快捷键上传当前选中的文本。

## 功能

- 在资源管理器经典右键菜单中选择“上传到文件交换系统”。
- 单文件或多文件按加入顺序逐个上传，显示字节进度、百分比、速度和服务器处理状态。
- 支持取消、失败重试和移除已结束任务。
- 按 `Ctrl+Alt+U` 获取当前选中文本，创建无 BOM 的 UTF-8 文本文件并上传。
- 尽力完整恢复快捷键使用前的剪贴板；无法完整恢复时给出明确提示。
- 支持一个或多个默认接收人域账号。
- 使用当前 Windows 用户作用域的 DPAPI 加密保存个人令牌。
- 单实例、托盘驻留；不要求管理员权限，也不会创建开机启动项。

Windows 11 的经典右键菜单入口通常位于“显示更多选项”中。

## 首次使用

1. 将 `GiantFileExchange.exe` 放到一个固定位置并运行。
2. 在“设置”页填写 API 基地址、个人令牌以及一个或多个默认接收人。
3. 确认传输方向。默认是“办公网 → 研发内网”，即 `exchangeType=2`。
4. 保存后，全局快捷键开始生效；首次运行还会为当前用户注册资源管理器右键菜单。

关闭主窗口只会将应用隐藏到托盘。要彻底结束应用，请使用托盘菜单中的“退出”；仍有任务时会要求确认。应用退出或异常关闭后不会恢复未完成任务。

便携程序移动到其他目录后，需在“设置”页点击“注册 / 修复”，让右键菜单指向新路径。

## 开发

要求：

- Windows 10/11 x64
- Rust stable，MSVC 工具链

```powershell
cargo check
cargo test --lib
cargo run
```

首次构建会下载并编译 egui/WGPU 等依赖，因此耗时较长。

## 发布

运行发布脚本：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/build-release.ps1
```

产物位于 `publish/GiantFileExchange.exe`。Release 构建启用 LTO、静态链接 MSVC CRT，并将版本信息和应用图标嵌入单个 EXE。

## 数据与接口

- 配置：`%LocalAppData%\GiantFileExchange\settings.json`
- 文本临时文件：`%LocalAppData%\GiantFileExchange\Temp`
- 上传诊断日志：`%LocalAppData%\GiantFileExchange\application.log`
- 崩溃日志：`%LocalAppData%\GiantFileExchange\crash.log`
- 上传接口：`POST /api/exchange/user/transfer/open/upload`
- multipart 字段：重复的 `receiverUser`、`file`、`exchangeType`
- 认证头：`Authorization: Bear {个人令牌}`

公司接口使用的是 `Bear`，不是标准 OAuth 常见的 `Bearer`。应用会自动纠正用户粘贴令牌时附带的 `Bear` 或 `Bearer` 前缀。

日志不会记录个人令牌或选中文本内容。上传任务不跨进程保存；失败的文本临时文件会保留到重试、移除或退出，其他文本临时文件会在任务结束时清理。

## 首版范围

当前版本只处理上传，不包含接收列表、文件下载、上传历史、开机自启或每次上传前的接收人确认。
