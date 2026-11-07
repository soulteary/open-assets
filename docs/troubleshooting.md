# 故障排查

先确认问题发生在哪一步：Push、Webhook、任务执行、公开读取还是管理登录。不要通过重跑初始化脚本重置环境。

## Push 被拒绝或连接失败

Git SSH 默认使用 2222，个人公钥需添加到 Gitea 账号并拥有仓库写权限。Publisher Deploy Key 是只读密钥，不能用于 Push。Stargate 网页密码也不是 SSH 密码。

核对仓库页显示的 clone URL、SSH 主机指纹、云安全组和主机防火墙。分支保护或非快进提交拒绝时先检查本地历史，不以 force push 处理所有错误。

## Push 成功但没有任务

查看 Gitea 仓库设置中的 Webhook 最近投递：

- 403：检查 Gitea 类型 Push 事件、仓库实际 ID、refs/heads 分支、enabled 和 Secret；Secret 必须与私有文件一致。
- 503：接收器处理失败或资源预算拒绝。响应包含 `resource_limit` 时，按[发布资源预算](resource-limits.md)检查容量，再重投事件；其他情况检查配置、目录权限和磁盘。
- 连接失败：确认 webhook 与 Gitea 在同一内部网络，Gitea 允许 webhook 目标，不要改成公网接收器。
- 202：任务已入队。按仓库 key 和 SHA 查询 status，不只看 Gitea 的“投递成功”。

相同 key、SHA、kind 已有任务时不会新增。已有 failed 任务需 retry。手动修改配置后重建 webhook/publisher，排除单文件挂载仍读取旧 inode 的情况。

## 任务 failed

先看任务 JSON 的 error 类型，再检查相关服务。程序有意不保存原始请求和完整异常：

- ResourceLimit：检查任务的 `resource_limit` 代码及[预算说明](resource-limits.md)，解除原因后再 retry。
- CalledProcessError：Git 初始化、拉取或提交历史检查失败；检查固定 remote、只读 Deploy Key、known_hosts 和进程资源限额。
- ValueError：可能是路径、图片扩展名、LFS、大小/数量限制、空发布、缺少 images、不可变版本冲突或空间预留不足；该类型不能单独确定具体原因。
- ClientError 等存储异常：检查内部 HTTPS/CA、发布账号是否启用以及目标前缀权限。
- PermissionError / OSError：核对数据目录所属 UID、挂载和磁盘状态。

不要在公开日志中临时打印整个环境、密钥或 SDK 请求。需要复现时，在私有维护环境中检查明确失败的阶段。修复原因后再 retry；failed 不会自动重试。

## 任务 superseded

自动任务的 SHA 已不是远端配置分支头。确认较新提交是否发布即可，通常不需重试旧任务。人工 publish 历史提交会改变当前图片，应在明确需要时使用。

## 图片 404 或仍是旧内容

核对目标任务是否 published、仓库 mount_path 是否匹配，以及是否错误地把 public/images 放进 URL。首次发布前没有 current，404 正常。serve=false 或 Nginx 映射未应用也会使图片不可见。

固定图片默认缓存 300 秒。同名文件更新后，浏览器或中间缓存可能暂时保留旧内容；需要固定内容时使用带 SHA 的附件 URL。没有目录列表，不能用“打开域名首页”判断所有文件是否可用。

## 附件 404 / 403

附件地址包含桶名、repositories、稳定 key、releases、完整 40 位 SHA，以及去掉 public/ 后的路径。图片地址与附件地址的拼接规则不同。

确认该版本确实发布成功，检查对象存在、匿名 GetObject 策略及 Traefik GET/HEAD 前缀规则。中文、空格、加号和百分号需要正确编码。公开入口不允许列桶或写入，不能把相应拒绝视为服务故障。

## 管理页面登录循环或 invalid_origin

确认认证配置中的实际主机名、精确 callback 允许列表和可信代理地址一致。检查仅四个管理主机匹配的 /_session_exchange 路由，以及受认证保护的 /__auth_return 路由；后者需返回固定同源导航文档。

OC 的正常页面登录流程与直接打开管理 API URL 不同。跨站直接打开旧下载地址可能被来源校验拒绝，不意味着登录页也应该绕过检查。不要删除 Origin/Sec-Fetch-*，不要关闭 CSRF 或证书验证来消除错误。

OC 需要启用的原生 IAM 用户，不接受 root、service-account 或 STS 作为 native 网页身份。退出 OC 后再刷新应回到 IAM 登录，Stargate 仍可能保留自己的会话。

## 接入 CLI 失败

错误输出包含阶段。先检查 [CLI 文档](cli.md) 中的前提：固定管理账号、OC alias/CA、专用策略和权限。前缀或稳定身份冲突应明确处理，不通过换一个随机 ID 绕过。

资源接入不是事务。失败后部分仓库、Key、Hook 或权限可能已存在，修复原因后重复同一 ensure 命令。private/onboarding 中的副本用于人工恢复参考；直接覆盖整份旧文件可能撤销其他仓库后续变更。

## 浏览器上传无法选择或发送文件

先区分应用错误与自动化工具、本机文件权限问题。项目的 Chrome 验收中曾遇到扩展未获文件 URL 权限，以及 Documents 路径文件在发送前中断；改用独立临时文件后上传成功。不要把这条环境记录当作所有上传错误的结论。

任务显示 100% 只表示浏览器发送完成，仍需查看服务端任务最终状态。下载可能打开新标签页，原页没有 download 事件不一定意味着文件没保存；检查下载结果并比对字节。

## Gitea 升级后 Hook 被出站策略拒绝

若投递记录提示私网目标需要 explicit IP allow entry，检查 Gitea 的 ALLOWED_HOST_LIST、WEBHOOK_IP 与独立 hooks 网络的实际地址是否一致。新版不仅检查名称，也检查解析后的非公网 IP。保留单目标白名单，不用放开全部私网来绕过。改正后重投失败事件或重新 Push，并等待对应任务 published。
