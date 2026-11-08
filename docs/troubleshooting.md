# 故障排查

先分清是哪一步出问题：连接 Git、Webhook 入队、任务发布、公开读取，还是管理登录。Push 成功、上传进度达到 100% 和页面能打开，分别只说明其中一个阶段完成。

下面的服务器命令在 `/app/open-assets` 执行。已进入 root shell 时可省略 `sudo`。不要用重跑初始化、删除任务记录或关闭认证检查来排查日常故障。

## 先收集当前状态

```sh
cd /app/open-assets
sudo ./scripts/open-assets doctor
sudo ./scripts/open-assets repo list
sudo ./scripts/open-assets status --key main-images
sudo docker compose ps
sudo docker compose logs --tail=100 publisher webhook
```

先从 repo list 找到实际 key，查询其他仓库时替换 `main-images`。保留目标完整 SHA、任务 ID、错误类型和发生时间；日志及报告只放在私有位置。doctor 的失败项带下一步建议，含义见[部署诊断](doctor.md)；它不检查每个业务文件，也不会自动修复。

## Push 被拒绝或连接失败

在自己的电脑、内容仓库中先查看连接目标与分支：

```sh
git remote -v
git status
git branch --show-current
```

1. 核对 remote 是否与 Gitea 仓库页的 SSH 地址一致，默认端口是 2222。检查云安全组和主机防火墙是否允许来源访问。
2. `Permission denied (publickey)`：确认个人公钥已添加到有仓库权限的 Gitea 账号，SSH 实际选用了对应私钥。Publisher Deploy Key 只读，Stargate 密码也不能用于 SSH。
3. 主机密钥变化：通过可信服务器会话核对新指纹，方法见[SSH 配置](usage.md#2-配置个人-git-ssh-密钥)。不要直接关闭主机密钥检查。
4. 非快进或分支保护拒绝：核对双方提交和权限，先解决冲突。空仓库首次提交不要拉取尚不存在的 main，按[首次发布](usage.md#首次发布空仓库)操作；日常错误不靠 force push 绕过。

## Push 成功但没有任务

在 Gitea 仓库设置中打开 Webhook 最近投递，先看响应：

- **403**：事件校验未通过。检查是否为 Gitea Push、实际仓库 ID、`refs/heads/<配置分支>`、`enabled` 和 Secret。Secret 与私有配置一致，不粘贴到排查输出。
- **503**：未确认接收。响应含 `resource_limit` 时按[预算说明](resource-limits.md)检查队列、Git 缓存和磁盘；其他情况检查接收器日志、配置和目录权限。
- **连接失败或私网目标被拒绝**：确认 Gitea 和 webhook 位于内部 hooks 网络。Gitea 的 `ALLOWED_HOST_LIST` 应允许目标主机和 `WEBHOOK_IP`；名称解析出的私网 IP 也需匹配。不要开放全部私网或改成公网接收器。
- **202**：任务已入队或命中已有任务，继续按 key 和 SHA 查状态，不只看“投递成功”。

查询活动与归档任务：

```sh
sudo ./scripts/open-assets status all --key main-images --sha "$ASSET_COMMIT"
```

先在服务器终端将 `ASSET_COMMIT` 设置为刚才 Push 的完整 40 位 SHA。相同 key、SHA、任务类型会去重，因此不一定产生新记录。已有 failed 任务应在修复原因后 retry。没有匹配项时再核对仓库身份、分支及配置是否加载，见[配置生效步骤](configuration.md#修改如何生效)。

修复投递原因后，在 Gitea 重投失败事件并查询状态；单纯再次 Push 未变化的分支不保证产生新的事件。

## 任务 failed 或长时间没有进展

先区分状态：`pending` 等待 Worker，`running` 正在执行，`images_prepared` 已准备图片但尚未完成发布。长期不变时检查 Publisher 健康、日志、磁盘和是否存在恢复或清理操作持锁；不要直接编辑任务状态。Worker 心跳正常也不证明目标任务已完成。

failed 任务结合私有任务 JSON 的 `error` 和阶段排查：

- `ResourceLimit`：查看 `resource_limit` 代码，检查对应预算和实际占用，不先把所有额度调大。
- `CalledProcessError`：检查固定 remote、只读 Deploy Key、known_hosts、分支历史和 Git 进程资源限制，不能仅凭此类型认定密钥失效。
- `ValueError`：核对路径、图片格式、LFS、文件数量与大小、是否缺少 images、不可变版本冲突及空间预留。默认发布需约 2 GiB 空闲，即使本次文件很小也要满足；该异常类型不能单独确定原因。
- `ClientError` 等存储异常：检查 OtterIO、内部 HTTPS/CA、发布账号状态和目标前缀权限。
- `PermissionError` / `OSError`：检查数据目录 UID、挂载、磁盘空间和 inode，不通过递归放宽权限解决。

修复原因后重试明确失败的任务：

```sh
sudo ./scripts/open-assets retry "$JOB_ID"
sudo ./scripts/open-assets status --key main-images
```

`JOB_ID` 使用 status 输出中的完整 64 位任务 ID，先在当前终端设置。failed 不自动重试，重复事件也不会重置它。程序有意不保存原始请求和完整异常，必要时在私有维护环境复现；不要临时向日志打印环境变量、密钥或完整 SDK 请求。

## 任务 superseded

自动任务的 SHA 已不是配置分支头。确认更新的提交是否已经 published，通常不必处理旧任务。人工 publish 历史提交可能切换当前图片，只在确实需要时执行；历史对象已清理时使用[恢复命令](object-restore.md)，不靠删除去重记录重新发布。

## 图片 404 或仍是旧内容

1. 确认目标 SHA 的任务为 published。首次发布前没有 current，图片 404 是预期情况。
2. 用[链接命令](operator.md#生成可复制链接)生成地址，检查图片前缀和文件名；图片 URL 不含 `public/images/`。没有目录列表，直接打开域名首页不能证明所有文件都不存在。
3. 检查 `serve` 与 Nginx 映射是否已应用。修改图片映射需要生成、校验并 reload Nginx；普通 Git 发布只切换当前目录。
4. 固定图片默认缓存 300 秒。需要比较当前响应时，可在自己的电脑请求具体图片：

```sh
curl --fail --head "$IMAGE_URL"
```

`IMAGE_URL` 使用刚才生成的完整图片地址。缓存头只说明响应策略，不证明内容字节一致；必要时下载后比对。需要引用固定版本时使用带 SHA 的附件地址，而不是依赖当前图片目录。

## 附件 404 / 403

附件路径包含桶名、`repositories`、稳定 key、`releases`、完整 SHA 和去掉 `public/` 后的文件路径，与图片 URL 规则不同。先使用 links 生成地址，再检查：

1. 目标版本是否发布成功，对象是否仍存在；历史清理实际删除后，旧链接会失效。
2. 是否使用公开 OSS 域名，而不是需要会话的 OC 下载链接。
3. 中文、空格、加号和百分号是否只编码一次。
4. 匿名 `GetObject` 策略和 Traefik GET/HEAD 前缀是否匹配。公开入口不允许列桶或写入，这类拒绝是预期行为。

需要重建已清理版本时按[对象恢复](object-restore.md)操作，不在 OC 中直接覆盖 Git 版本目录。

## 管理页面登录循环或 invalid_origin

先从 `private/domains.json` 中的实际管理首页登录，不使用旧书签中的下载或 API 地址：

1. 确认公网 HTTPS 证书有效、域名与页面一致；证书错误先修复信任或签发，不跳过浏览器警告。
2. 核对 Stargate 精确 callback 允许列表、可信代理和四个受保护管理主机。`/_session_exchange` 仅匹配指定主机，`/__auth_return` 需受认证保护并返回固定同源导航页面。
3. 在服务器检查组件状态与最近日志：

```sh
sudo docker compose ps stargate oc nginx
sudo docker compose logs --tail=100 stargate oc nginx
```

4. Stargate 通过后，OC 仍需要启用的原生 IAM 用户。root、service-account 或 STS 不能作为 native 网页身份。退出 OC 后刷新应回到 IAM 登录，Stargate 可能仍保留自己的会话。

跨站直接打开旧下载地址可能被来源检查拒绝。不要删除 Origin/Sec-Fetch-*，不要关闭 CSRF 或后端证书验证来消除错误。路由要求见[部署说明](deployment.md#5-生成路由并核对-https)。

## 接入 CLI 失败

错误包含执行阶段，按阶段定位：

- `preflight`：检查 key、仓库绑定、分支、图片前缀和私有配置。身份或前缀冲突应明确处理，不随意换随机 key 绕过。
- `gitea-repository` / `deploy-key` / `webhook`：检查管理账号权限、仓库是否私有且未归档、已有 Key 是否只读、有无重复 Hook。
- `s3-policy`：检查 OC alias、CA、专用发布策略和发布账号是否启用，不给 Publisher 改用 root 或删除权限。
- `configuration-and-nginx`：检查 YAML、目录权限、Nginx 配置测试和服务重建结果，保留原配置及接入记录。

完整前提见[CLI 文档](cli.md)。资源接入不是事务，失败后部分仓库、Key、Hook 或权限可能已存在。修复根因后重复同一 ensure 命令；`private/onboarding/` 的备份只作人工恢复参考，直接覆盖旧文件可能撤销其他仓库后续变更。

## 网页上传或下载失败

先区分选择文件、发送、服务端写入和下载四个阶段：

1. 无法选择文件：先用浏览器手动操作和一份无敏感的小文件测试，检查本机文件是否可读；自动化工具故障不一定是服务故障。
2. 请求返回 401/403：检查 Stargate 和 OC 会话、IAM 权限及是否从 OC 正常页面发起上传。
3. 发送中断或服务端报错：查看浏览器网络请求的状态码，同时检查 OC/OtterIO 健康、日志和磁盘。不要公开请求中的 Cookie、Authorization 或签名 URL。
4. 上传进度 100% 只表示发送阶段的进度，还要确认 OC 中对象出现、大小正确并能下载。网页上传没有 Publisher 的 published 任务，也不会更新 Git 图片目录。

下载可能打开新标签页。确认浏览器实际保存的文件及字节，不仅看页面是否提示完成。网页管理与公开分享的区别见[使用指南](usage.md#通过网页管理文件)。
