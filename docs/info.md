# Info 运行看板

默认地址为 `https://info-node-name.console.lab.dev`。以私有 `domains.json` 中的实际地址为准。所有页面、静态资源和 `/api/status` 都经过 Stargate ForwardAuth；会话交换与认证落地页沿用精确管理主机路由。

## 展示内容与数据边界

看板展示主机 CPU 总利用率、逻辑核数、可用内存、Swap、系统负载与运行时间；组件的运行状态、Docker 健康状态、可用的 HTTP/TLS/心跳探测、CPU、内存工作集和重启次数；磁盘可用容量与 inode；各仓库的 OtterIO 对象数与逻辑字节数；内部 CA/叶证书及公网证书的到期时间。如果存在限量压测结果，还会展示请求数、错误数、吞吐和延迟分位数。

磁盘区独占一行，证书区在桌面宽度下分为内网与 ACME 公网两列，小屏幕顺序排列。顶部“认证状态”进入配置的 Stargate 认证主机，仅显示当前认证状态，不是退出按钮；不接收请求参数指定的返回地址。恢复记录仅展示固定字段，是否成功恢复仍以组件当前健康状态为准。

Docker CPU 的 100% 表示使用一个逻辑核；主机 CPU 则以整机为 100%，两者不能直接混用。内存工作集不是进程 RSS，也不包含主机上全部文件缓存。磁盘 `/` 与数据目录位于同一文件系统时只显示一项，不能把同一容量加两次。图片、发布状态或对象数据若使用独立挂载，会分别展示；发布预留按图片盘的可用空间判断。

宿主机 collector 有受信任运维权限，读取 Docker 状态、Linux `/proc` 和公开证书，生成明确字段的 JSON 快照。它不输出环境变量、密码、SSH 密钥、容器完整 inspect、主机名、公网 IP 或完整文件路径。版本镜像引用和系统版本属于运行信息，仍只供管理入口访问。

info 容器以非 root 身份运行，根文件系统只读，只有快照目录的只读挂载；不挂载 Docker socket、宿主机 `/proc`、private 或证书私钥。它提供固定读取路由，不提供 PHP 探针中的命令执行、文件管理、端口扫描或在线性能测试功能。

## 启动采集

完成部署、DNS 和 Traefik 接入后，在服务器管理员 shell 执行：

```sh
cd /app/open-assets
install -d -o root -g 20010 -m 0750 /data/asset-stack/info
docker compose up -d --no-build info
python3 -m open_assets.telemetry --root /app/open-assets
install -m 0644 config/systemd/open-assets-info.service /etc/systemd/system/
install -m 0644 config/systemd/open-assets-info.timer /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now open-assets-info.timer
systemctl status open-assets-info.timer --no-pager
```

固定路径与 Compose 项目限制仍适用。采集每次结束后约 30 秒再运行，页面每 30 秒读取一次；超过 120 秒的快照会标记过期。采集失败保留旧快照，不能把旧快照当作实时健康证明。定时器的状态与错误通过 systemd 日志检查：

```sh
journalctl -u open-assets-info.service --since '10 minutes ago' --no-pager
```

`/healthz` 只表示 info HTTP 进程存活。Publisher 的健康心跳不证明最近任务成功；Traefik 的 ping 不等于所有入口配置正确。判断具体发布还应使用任务状态与公开字节验证。各探针与自动恢复边界见 [健康检查](health.md)。

## 已有部署增加入口

先备份应用与现有 Traefik 私有配置，按 [升级说明](upgrade.md) 更新源码。根据 `.env` 中的节点与基域生成 info 地址，追加到 `private/domains.json` 和 Stargate 的 `CALLBACK_ALLOWED_HOSTS`，保留原有域名、密码与交换密钥。重新生成现有 Traefik 的动态文件，重建 Stargate 和 info，再安装采集定时器。

四个受保护管理主机都必须包含精确 `/_session_exchange` 与受保护 `/__auth_return` 路由。不要只新增 info 主路由而漏掉回调与交换路径，也不要把认证例外改成全局规则。匿名访问 `/`、`/api/status`、`/app.js` 均应进入 Stargate；登录后页面与 API 应可读，会话失效后重新受到保护。

内部证书长期有效不等于可以忽略私钥保护；到期提示也不替代异机告警。原始性能采样、私有快照与带实际地址的截图不要提交 GitHub。

Docker 重启计数来自 Docker `RestartCount`，不包含所有手工或宿主机修复程序发起的重启。自动修复另有受限的操作记录，不能只靠该计数判断是否修复过。

组件状态按“运行 / 健康 / 探测”展示：运行状态来自 Docker State，健康来自 Docker Healthcheck，探测是宿主机采集器额外执行的请求或心跳检查。未配置额外探测时省略第三项及其分隔符。目前 Traefik 使用容器内 `traefik healthcheck`，没有额外 HTTP 探针，因此正常显示为 `running / healthy`；其他组件显示 `ok` 是其额外探测通过。这些检查均不保证所有公开路由或发布结果正确。

受保护的 Gitea、OC、Traefik 和 Info 路由按顺序使用 `console-auth` 与 `console-gzip` 中间件。认证通过后，对客户端接受 gzip 的 HTML、CSS、JavaScript、JSON、XML 和 SVG 响应启用 gzip，最小响应体 1024 字节。未通过认证的响应、Stargate 登录与会话交换路由不添加此压缩中间件，公开图片和对象路由保持原配置。已经编码、体积较小或不属于允许文本类型的响应不要求压缩。配置依据见 [Traefik Compress](https://doc.traefik.io/traefik/reference/routing-configuration/http/middlewares/compress/)。

## 仓库容量、保留策略和清理结果

“仓库容量与保留”按稳定 key 展示仓库，不输出 Git remote、真实目录、完整 SHA、任务 ID、密钥或原始配置。

- 本地版本按 releases 中可识别的 SHA 去重计数，完整 public 与 images 副本分别计数，未配对目录单独标记。这只是目录统计，不代表发布成功或内容校验通过；发布中的瞬间也可能出现未配对目录。
- 本地占用包括仓库图片目录下的版本、清理暂存、发布暂存和其他本地条目，加上该仓库 Git 缓存与清单。按 `st_blocks × 512` 估算分配空间，仓库内硬链接去重，不跟随软链接，current 不重复计入目标内容。
- 本地占用不包括 OSS 对象、共享任务/归档/隔离数据、备份、Docker 镜像与日志，也不能把各仓库值当作整机磁盘使用量。文件系统压缩、共享块与元数据可能使估算和实际可回收空间不同。
- 保留策略显示是否启用、数量/天数并集、安全间隔、回滚与固定版本数、每轮上限及任务归档规则。固定 SHA 只输出数量，回滚、当前和在途任务的实际保护仍以清理程序重新计算为准。
- 最近完整清理优先读取每仓库 retention-results/<key>.json；升级前记录回退到 retention-last.json：完成时间、移入清理暂存的版本数、删除的暂存目录数和归档任务数。目录数不是版本数，也不等于已释放的磁盘容量。未启用仓库不把预览候选计为已删除。

定时服务状态单独展示。最近失败或正在运行时，会明确说明下表是上次完整结果；进程正常退出也可能是持锁忙碌跳过，不代表完成了清理。每仓库摘要不会被其他仓库的单独执行覆盖。尚无结果、旧格式最近一次未包含该仓库、元数据损坏均有独立状态，不显示成“删除 0 个且成功”。

常规主机采集器读取固定 DATA_ROOT 映射的本地目录和对象统计缓存，不调用 S3、不读取对象内容、不执行 Git 或清理操作。统计默认缓存 300 秒；配置、版本目录、发布记录或清理结果变化时提前刷新。每行显示实际统计时间。至多支持 100 个配置仓库，扫描共享 2 秒检查预算和 50,000 条目录项上限；超限或出错时保留旧测量并标记不可用/旧数据，不用零代替未知值。该预算不能中断阻塞的内核 I/O。

缓存位于 info 数据目录的 repositories-cache.json，权限为 0600。Info HTTP 进程不能读取该文件；公开给受保护页面的 snapshot.json 仍只包含明确字段。新增统计仅支持既有 `/state`、`/images` 容器路径映射，其他映射会提示不可用。

需要立即刷新统计时在管理员 shell 执行：

```sh
cd /app/open-assets
sudo python3 -m open_assets.telemetry --root /app/open-assets --refresh-storage
```

## 容量提示

阈值在私有 .env 配置，采集器下次运行时读取，不必因此重建应用容器：

```dotenv
INFO_DISK_WARN_PERCENT=85
INFO_DISK_CRITICAL_PERCENT=95
INFO_DISK_WARN_FREE_BYTES=5368709120
INFO_DISK_CRITICAL_FREE_BYTES=2147483648
INFO_INODE_WARN_PERCENT=85
INFO_INODE_CRITICAL_PERCENT=95
```

空间或 inode 使用率达到阈值时提示；可用空间低于字节阈值时提示。默认分别为 85% / 95%、5 GiB / 2 GiB。数据盘可用空间小于任一仓库 `max_bytes × 2` 时，还会提示该仓库无法满足现有 Publisher 的发布预留检查。这是配置上限预留，不是此次上传文件的预测大小。

阈值无效、inode 数据未知、仓库统计不可用或过期时，不显示“全部正常”；整个快照超过 120 秒或浏览器刷新失败时，也会撤销实时容量正常提示。提示不会自动删除、解锁、重启或发送通知，不能替代独立的外部容量告警。若没有可清理版本，应检查 Git、OSS、任务归档、备份和日志等其他占用，不能突破 current/回滚/在途任务的保护强制清理。

开发验证可运行 `python -m unittest discover -s tests -v` 和 `node --test tests/test_storage_ui.js`。前者覆盖采集和数据边界，后者用最小 DOM 替身验证渲染、旧数据、读取失败和文本注入；实际浏览器布局仍需单独检查。

## OtterIO 对象统计

仓库表中的“OtterIO 对象”独立于本地容量，按配置桶内 `repositories/<key>/` 前缀递归统计，包含该前缀下的历史附件、清单和未完成发布已上传的对象，也包含已禁用但仍在配置中的仓库。相邻 key 不共享前缀；配置之外的对象不计入。新增白名单仓库后，下次对象采集自动识别，不需修改服务或扩大现有 Publisher 权限。

对象数量来自 ListObjectsV2 的 Contents 项数，字节数为 Size 之和，包括零字节对象。不下载对象，不逐对象 HEAD，不枚举桶根路径。该 API 需要既有 Publisher 账号的 s3:ListBucket 权限；沿用其内部 HTTPS、CA 与凭据，不使用存储 root 账号。执行程序只有列表调用，但复用的 Publisher 身份本身仍具有发布写入权限，并非新增的只读 IAM 身份。

这里的大小是对象逻辑大小，不是 OtterIO 的文件系统块、冗余、索引、分片暂存或可回收磁盘空间。仅统计列表中当前可见的对象，不枚举 S3 非当前版本、删除标记和未完成分片。它与本地占用分别展示，不能相加作为磁盘用量。多页或跨轮列表不是事务快照；并发发布或外部修改期间的结果是采集时间范围内的近似统计，不能用来做计费、内容完整性校验或删除判据。[ListObjectsV2 语义](https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListObjectsV2.html)

### 安装独立定时器

先按升级流程构建并更新 Publisher 和 Info 镜像，使容器包含 object_info 模块和页面，再安装：

```sh
cd /app/open-assets
sudo install -m 0644 config/systemd/open-assets-objects.service /etc/systemd/system/
sudo install -m 0644 config/systemd/open-assets-objects.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl start open-assets-objects.service
sudo systemctl enable --now open-assets-objects.timer
sudo systemctl status open-assets-objects.timer --no-pager
```

独立 oneshot 服务通过 docker compose exec 在 Publisher 内执行元数据采集，结果经宿主机校验后原子写入 info/objects-cache.json，权限为 root 0600。续页 token 只在私有缓存和内部进程间传输；对页面仅输出稳定 key、对象数、字节数、状态和起止时间，不输出对象名、桶名、endpoint、token 或异常原文。Info 容器无法读取该缓存，只能读取常规采集器过滤后的 snapshot.json。采集不持有发布锁，不改对象、IAM、任务或清理策略。

首次启动约 2 分钟后执行，此后每次结束约 15 分钟再运行。每仓库距上次尝试不足 900 秒时跳过，即使重复手工启动也不会反复扫描。每轮全仓库共享 100 次列表请求与 110 秒检查预算，每页最多 1000 个对象，请求起点间隔至少 1 秒；连接/读取超时分别为 2/5 秒，关闭 SDK 自动重试。宿主机等待最多 130 秒，systemd 最多 140 秒。预算检查不能中断已开始的阻塞 I/O。

仓库间轮流取页；预算耗尽后保存累计值和续页 token，下轮继续。未扫完不发布部分总数，而是显示“分页采集中”及上次完整结果；首次尚无完整结果时显示未知。续采超过 24 小时会重新开始，避免无限沿用旧扫描；此上限也意味着极大前缀可能长期无法完成，需要降低对象规模或另行设计离线统计，不能仅提高刷新频率。

失败时保留上次完整结果并显示失败；该仓库出错的续页进度丢弃，下轮重新扫描。完整结果超过 30 分钟标记过期。桶、endpoint 或仓库 key 集合变化时失效旧对象缓存；保留策略改变不导致额外遍历。磁盘容量提示仍依据实际文件系统和本地发布预留，不把对象逻辑大小当作磁盘空间依据。

### 检查与手工触发

```sh
sudo systemctl start open-assets-objects.service
sudo journalctl -u open-assets-objects.service --since '1 hour ago' --no-pager
sudo systemctl list-timers open-assets-objects.timer
```

手工启动同样受缓存间隔限制，不提供绕过限速的网页刷新入口。结果由下一轮常规主机采集进入看板；如需马上合并已有对象缓存，执行 sudo python3 -m open_assets.telemetry。此命令及 --refresh-storage 都不会触发 S3 请求。全进程失败或超时也保留旧完整值；先看服务日志和每行状态，再检查 Publisher 健康、内部证书及列表权限，避免删除缓存来反复重试。

对象历史清理采用独立策略和专用维护账号，默认定时预览，详见[对象保留](object-retention.md)。对象统计会在下一轮完整采集后反映实际删除；本地“最近清理”不包含对象清理结果。

## 对象保留策略与清理记录

“OtterIO 对象保留”独占一行，与上方的本地保留和本地清理区分。页面展示当前配置中的数量、天数、安全间隔、单轮版本/对象/逻辑字节预算、独占写入声明、回滚保护数量与两类固定 SHA 的去重数量。策略启用不代表已经授予 IAM 删除权限，也不代表定时任务带有 --apply；看板不会检查或展示凭据。

候选统计来自最近完整对象规划，不在网页或 30 秒主机采集时调用 S3：

- “待清理”总量是当轮未被引用、保留数量/时间或安全条件保护的版本及其对象、逻辑字节；“单轮候选”另显示预算内选中的部分。两者不是整桶容量，也不保证每个候选的删除前 HEAD 校验都能通过。
- 规划时间与模式均单独标注。实际执行产生的规划是删除前快照，不能把它当成删除后的实时剩余量。需要更新候选时，按对象保留文档执行一次只读预览；浏览器刷新不重新扫描。
- 保护原因按受保护 SHA 去重计数，不是任务数量，各原因之间可以重叠；还会展示未知对象数量和因策略/请求预算排除或跳过的版本数。未知对象不会因此成为自动删除候选。
- 完整规划超过 12 小时或共享保留配置发生变化时标记过期/需重新预览。历史执行记录仍可阅读，但不是对当前状态的保证。

最近一次尝试和两类完整历史分别保存：

- 预览明确显示“仅预览，未调用对象删除”；执行请求如果策略未启用，会说明跳过原因，不能称为实际删除成功。
- 完整执行展示已确认删除的版本、对象与逻辑字节；这不是磁盘实际释放量。后续预览不会覆盖最近完整执行，单仓库运行也不会覆盖其他仓库摘要。
- 锁忙、未声明独占写入、桶版本控制、扫描/时间预算、权限失败、引用或元数据冲突、存储/I/O 错误使用固定原因码展示。不输出 SDK 异常原文、对象名、完整 SHA、桶名、endpoint 或 token。
- 失败或未结案执行可能已删除部分对象；有响应确认的数量单列，未收到响应的删除无法准确计入。最近完整执行不是这次失败的结果。进程被杀、写摘要失败或主机中断时，可能只留下“尝试未结案”，应结合服务状态、私有日志及删除收据排查，不能据此断定未删除。

摘要位于 state_root/object-retention-results/<key>.json，包含 attempt、last_plan、last_preview、last_apply。宿主机按固定 DATA_ROOT/publisher 映射读取并过滤到状态快照；Info 容器仍不挂载发布状态目录。结果缺失、损坏、未知与零候选分开显示。升级前的单份 object-retention-last.json 不自动冒充这四类历史，升级后需执行一次预览初始化摘要。

定时服务状态仅表示进程运行/退出情况，不能证明已经删除。页面只读，没有预览、重试或删除按钮。默认六小时预览和手工操作步骤见[对象保留](object-retention.md)。

## 最近恢复结果

“最近恢复结果”按仓库展示 `restore-objects` 的持久化报告，包括对象读回校验、本地 public 与图片副本校验、本次是否安装副本，以及是否请求并完成显式回滚。默认恢复不切换当前版本；看板不提供恢复或回滚按钮。命令见[恢复 CLI](object-restore.md)。

每仓库选择 `started_at` 最近的一份报告，不按文件修改时间或结束时间选择。相同 SHA 的重试覆盖该 SHA 上次记录，因此这不是完整审计历史。获取维护锁失败的 busy、参数错误等发生在报告创建前的拒绝不会生成恢复报告。没有记录显示“尚无恢复记录”，不是恢复成功。

- 对象校验通过表示该次恢复已完成全部对象读回核验；即使后续本地安装失败，也保留这一阶段结果。本地校验通过表示两份副本均核验通过，安装标志仅说明本次是否重建，重复执行可能两项均为否。
- “显式回滚完成（当时）”仅来自 `activated=true`，不是当前版本指示。后续正常发布可以再次切换。旧报告没有本地或回滚字段时显示未记录，不推断其取值。
- failed 显示失败阶段和有限类别（校验、文件读写、权限、缺失、超时、Git、存储、中断或未分类），不公开原始异常、请求或文件名。详细原因需核对私有恢复报告。
- running 也可能是进程中断后未结案，不据此推断进程仍在运行。存在部分写入/安装可能或 `activation_uncertain` 时显示状态不确定；应检查对象与 current/published。`activated=false` 不证明从未临时切换。
- 存储 endpoint/bucket 范围变更会提示历史结果不证明当前存储有效。所有校验都是恢复当时的结果，不持续验证外部对象和文件，也不撤销过去的清理记录。

摘要由现有主机采集定时器只读生成，在下一次成功采样后显示；浏览器刷新不遍历 Git、对象或图片目录。每仓库最多检查 512 个目录条目，每份报告最多 16 KiB，总扫描预算两秒。异常报告、目录链接、未知条目、超限或超时会显示“报告不可用”，不回退到较旧成功报告。原子写入的临时文件不作为报告。报告过多时应在确认没有维护进程后归档到私有备份，避免直接删除审计材料。

公开 API 摘要只包含仓库 key、状态、有限阶段/失败类别、计数、时间和布尔标志，不含完整 SHA、对象名、远端地址、凭据或报告原始错误。API 仍受 Stargate 保护。采集过期或刷新失败会提示旧数据；浏览器与主机采集都不修改恢复报告。

## 恢复状态核对

“恢复状态核对”展示显式 `check-restore` 的最近报告，包括对象/public/图片、current/published 一致性、目标是否当前在用、对象清理屏障及原恢复是否不确定。命令与退出码见[核对 CLI](restore-check.md)。当前在用不等于过去回滚成功；原恢复未结案也不会因核对通过自动结案。

每仓库按核对 started_at 选择最新报告，可能与“最近恢复结果”的目标不同。没有记录、坏记录、未结案、检查失败和屏障阻止均独立展示，不回退显示旧成功。摘要沿用每仓库 512 项、每文件 16 KiB、全局两秒读取上限；原子临时文件除外。核对报告与原报告都需纳入私有备份和人工归档。

看板标明核对时点。超过 24 小时，或发现存储范围、原恢复报告、清单、current/published 摘要变化时，提示重新核对；无法比较时提示人工核查。摘要比较不验证对象和目录是否随后损坏，即使没有变化提示也不能视为实时完整性保证。API 仅暴露有限状态、原因、时间、计数及布尔标志，不暴露完整 SHA、摘要值、对象名或原始错误；仍受 Stargate 保护。浏览器刷新和主机采集不修改核对证据，也不会自动运行核对 CLI。

## 恢复结案记录

“恢复结案记录”只读显示 [close-restore](restore-close.md) 的独立结果。它不覆盖“最近恢复结果”或“恢复状态核对”，所以原报告仍可能显示未结案/不确定。成功表示结案当时重新核对通过并显式确认，不证明历史切换成功，不表示内容被永久保留。

每仓库按 started_at 选择最近尝试，目标可能与最近恢复/核对不同。拒绝、失败、未完成 intent 和坏记录不会回退为旧成功。最近尝试为 closed 时，还会验证其原始报告与核对快照的摘要和绑定关系；缺失或损坏显示不可确认。intent/result 各限制 8 KiB、证据快照各限制 16 KiB、每仓库最多 512 个尝试目录，总扫描预算两秒。

页面区分核对时目标当前在用和保留其他当前版本。若当前原报告、存储范围、清单或 current/published 与结案时不同，显示历史记录及变化提示；无法比较时要求人工核对。提示不进行对象遍历或重新校验所有文件。原报告被同 SHA 的后续恢复更新时，旧结案不覆盖新尝试，但私有 source.json 仍保留旧报告。

API 不暴露 SHA、审计 ID、执行 UID、自由文本原因、摘要值、对象名或原始错误。完整记录仅在私有状态目录，仍需独立备份。浏览器刷新和主机采集不修改审计、不运行结案 CLI；访问继续受 Stargate 保护。
