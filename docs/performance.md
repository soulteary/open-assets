# 资源采集、限量压测与缓存选择

## 如何测量

性能数据必须注明场景、样本数、并发、请求速率上限和耗时。Docker CPU 的 100% 表示一个逻辑核；主机 CPU 的 100% 表示整机。Docker 内存工作集、进程 RSS、主机可用内存与 buff/cache 不是同一个指标，不能互相替代或直接相加。

本项目提供宿主机只读采样与限量读取测试。在可信管理员 shell 执行：

```sh
cd /app/open-assets
bin/open-assets benchmark --baseline --seconds 20
```

资源原始采样写入 `private/performance/baseline.json`。`info` 定时采集程序也会持续提供 CPU、内存和容量快照，但不是长期时序数据库。

读取测试需要一个已经发布、同时包含图片与普通附件的专用测试仓库。不要选取生产大文件或包含秘密的内容。使用 repo ensure 接入、个人 SSH Key Push 并等待 published 后，再执行：

```sh
bin/open-assets benchmark \
  --key load-test \
  --seconds 10 \
  --max-rps 40 \
  --concurrency 1 4 8
```

测试通过回环地址访问真实 HTTPS 入口，保留 SNI、证书验证与 Host，经 Traefik 进入 Nginx/OtterIO。每个 Worker 复用自己的连接，每个响应都核对状态码、字节数上限及 SHA-256。只读图片与版本附件，不进行删除或批量写入；输入文件限制在 16 MiB 以内。

每场景目标持续时间最多 30 秒（已发出的请求还需等待其超时或完成）、并发最多 8、总请求速率最多 100 req/s，默认 40 req/s。启动和运行中检查可用内存、磁盘与系统负载；低于 512 MiB 可用内存、低于 2 GiB 空闲盘或一分钟负载超过逻辑核数的两倍时终止。累计错误会提前停止。保护检查不是生产压测的完整隔离措施，执行时仍应观察已有服务。

结果保存到 `private/performance/latest-summary.json`，包含请求数、错误数、吞吐、p50/p95/p99 和资源采样。Info 只读取其中明确的摘要字段。该测试有请求速率上限；达到上限不表示达到服务器容量上限。客户端与服务共享 CPU，回环测试不覆盖公网带宽、用户网络、跨区域延迟和 CDN。不要据此承诺公网并发容量。

## Nginx 的优化边界

Nginx 当前负责本地图片读取，TLS 在 Traefik 终止，附件仍由 OtterIO 提供。应分开观察这三个组件的 CPU 与延迟，不能把所有读取延迟归因于 Nginx。

可以测试的低复杂度选项是 `sendfile on`、适量 Worker、连接复用及适合实际并发的 worker_connections。它们可能降低用户态拷贝或减少连接开销，但低负载、热缓存、小文件场景不一定有可测收益。对比时保留相同文件、并发、速率上限和缓存条件，记录字节、Range 与图片切换是否正确；不要仅报告“优化后更快”。选项含义见 [Nginx core module](https://nginx.org/en/docs/http/ngx_http_core_module.html) 与 [core 配置](https://nginx.org/en/docs/ngx_core_module.html)。

当前保留 `open_file_cache off`。固定图片 URL 经过 current 软链接切换，缓存文件描述符或元数据可能继续使用旧版本；不能为了减少 stat 就牺牲切换语义。`sendfile` 与内核页缓存不等同于 open_file_cache。

PNG/JPEG/WebP/AVIF、压缩包等通常已有压缩，不默认对它们再次 gzip。固定图片 URL 继续使用短缓存与重新验证；只有带 SHA 的不可变附件适合长缓存。对同名图片要求立即更新时，应采用版本 URL 或更严格的重新验证策略，而不是把固定 URL 标成 immutable。

## 是否需要 Redis、Memcached 或其他内存缓存

是否增加缓存，应先采样目标机器的可用内存、读取延迟和流量。项目默认不增加 Redis/Memcached；内核页缓存和 HTTP 缓存已承担一部分重复读取。[历史采样](history/2026-11-07-performance.md)仅供比较测量口径。

当前不增加独立内存缓存服务。历史采样节点的数据量较小、内存有余量，未观察到需要独立缓存解决的瓶颈；这不代表其他部署的容量或性能。是否增加缓存应以目标机器的采样和命中率为依据。MemAvailable 是内核对可供新进程使用内存的估算，不能再把全部 buff/cache 加到它上面。页缓存机制见 [Linux 内存概念](https://docs.kernel.org/admin-guide/mm/concepts.html)。

Redis/Memcached 不会在启动后自动加速 Nginx 文件或 S3 对象。接入还需要缓存协议、对象序列化或模块、容量与淘汰策略、身份边界，以及固定图片 URL 的更新/回滚失效机制；二进制文件还会增加重复数据与复制开销。Redis 作为多实例会话存储又是另一个用途，不能把会话存储和资源缓存混为一谈。

后续若持续观测到重复对象读取使 OtterIO CPU 或磁盘成为瓶颈，可先评估公开、不可变附件的 CDN 或带容量上限的反向代理缓存；管理接口、带身份请求和未完成版本不应混入公开缓存。涉及代理缓存时另测 Range/206、Cache-Control、错误响应及缓存键，不能直接照搬图片配置。若数据热集显著超过页缓存，再比较缓存命中率、后端减载与额外内存的收益。

生产方案应给 Git、发布 Worker、存储、系统页缓存和构建/升级留出余量，不以“还有空闲内存”为理由启动一个没有明确命中率和失效策略的缓存组件。

## 当前 Nginx 配置

配方使用自动 Worker、1024 个 worker_connections、sendfile、tcp_nopush、tcp_nodelay 和 30 秒 keepalive，保持 open_file_cache 关闭。更改后应检查图片切换、历史附件、Range 与完整字节；用相同样本对比延迟和资源消耗。历史样本及结果见[采样记录](history/2026-11-07-performance.md)。

## Publisher 的批量读取与并发发布

每个新提交仍生成完整、独立的发布版本，不改变含 SHA 的对象 URL、清单格式、恢复屏障或原子图片切换。Git 使用一次 `ls-tree -rlz` 检查完整树的路径、类型和容量预算，再使用一个 `cat-file --batch` 子进程流式读取原始 blob；不 checkout，不执行过滤器、仓库脚本或 LFS。批量协议逐对象核对 OID、类型、长度和分隔符，仍受 Git 进程内存限制及读取超时控制；导出后保留 SHA-256、LFS 和静态 SVG 验证。

只有整批内容验证完成后才开始 S3 写入。最多同时处理 4 个对象，每个对象仍需上传后完整读回校验；已有对象重放只读取校验，不覆盖不匹配的字节。失败后停止调度新对象、等待在途工作结束，保持旧图片版本。部分不可变对象可能保留，按现有保留策略处理，不自动删除。

可在 `private/repositories.yaml` 顶层设置：

```yaml
publication:
  object_workers: 4
```

允许 1 至 8 的整数，默认 4；2 核机器建议从 2 或 4 开始，以实际 CPU、磁盘、发布耗时和在线读取延迟对比。待执行对象数量也有并发上限，不一次提交整个仓库的 futures。该参数不增加多个发布 Worker，跨版本切换仍由单 Worker 和既有文件锁保护。

本地图片视图优先硬链接到同一发布版本的完整快照，跨文件系统或不支持硬链接时退回复制。不同提交不共享 inode，不对旧快照原地修改；只用既有的发布、回退和清理命令维护版本目录。删除同一版本的一个目录不会删除另一视图仍引用的文件；但对硬链接文件原地改写会同时损坏该版本的两份视图，校验会拒绝其回退，不能将它们当作两份独立备份。仍须保留 Git、对象存储和异地备份。容量采样中的逻辑字节数可能大于实际物理占用。

完成任务的 `timings` 记录 fetch、export、objects、snapshot、activation 和 total 的秒数。对比完整首次发布时应使用内容相同但 SHA 不同的新提交；重试同一 SHA 会复用已有对象和本地快照，不能冒充完整发布速度。历史 40 req/s 读取测试不代表 QPS 上限。

## 少量文件更新的增量复用

Publisher 在完整校验和激活后保存可丢弃的增量凭据：绑定当前版本、完整清单摘要、验证配置、Git blob OID、本地 inode/大小/mtime/ctime，以及完整读回时由实际字节计算并与 S3 ETag 一致的 MD5 和源对象修改时间。首次升级、凭据缺失或失效时退回完整读取与上传，不放宽 SVG、LFS、隐藏路径和容量规则。文件改名为 SVG 时仍重新做 SVG 校验。

后续版本的未变 blob 从已验证快照复制成独立 inode，不再从 Git 导出和重复计算其内容摘要。对象存储使用 `CopySourceIfMatch` 和 `CopySourceIfUnmodifiedSince` 固定先前已完整 SHA-256 验证的不可变源对象身份，执行服务器端拷贝并核对返回 ETag。响应提供原生 SHA-256 时也必须匹配清单；不提供时，只允许续用包含源修改时间的完整验证凭据，缺少该证据仍退回完整读回。新建或变化内容始终完整上传并读回做 SHA-256 验证。ETag 只用于有验证来源的对象身份条件，不将任意 HEAD 元数据或未经验证的已有对象当作内容哈希。缺失、变化或拷贝证据不一致会拒绝激活。源版本不与新版本共享对象路径，清理源版本不会破坏后续版本，回退和 Git 恢复继续使用完整清单。

这减少了大文件的客户端传输与重复解析，不等于每次更新只执行变化文件数量的操作：版本化 URL 仍要求为未变对象建立新版本的服务器端副本，本地目录也保持独立。耗时仍随文件总数和存储元数据性能增长，不能承诺固定秒数。图片激活前后保留全树字节校验；增量凭据不是抵御磁盘静默损坏的独立备份。对象恢复或显式回退之后不匹配的凭据自动失效。

增量模式沿用发布命名空间由单一 Publisher 维护、版本对象不可原地编辑的约定。验证凭据由成功内容校验生成，绑定桶/endpoint 配置、版本和清单；不是用户提供的远端缓存。兼容存储必须正确执行条件拷贝。源对象条件不匹配、返回 ETag 或原生 SHA-256 冲突都停止激活，不能静默覆盖冲突；完整对象恢复流程仍重新读取 Git 和验证全部对象。

## 引用式版本发布（可选）

`publication.reference_releases: true` 需要同时部署 `object-reader`，并把现有 OSS 版本路由指向它；图片仍由原 Nginx 路由服务。外部 `/attachments/repositories/<key>/releases/<sha>/<path>` URL 不变，内部按完整清单直接读取最初验证过的对象，引用被展平成直接对象地址，不建立递归链。旧完整版本和没有本地清单的旧 URL 继续兼容。读取组件只挂载公开清单、清理收据和 CA，只能向固定 S3 地址发起匿名 GET/HEAD；不持有密钥、Docker socket 或写权限。Range、HEAD、ETag/304、MIME 和 CSP 均须经过升级验收。

未变对象只进行按源版本分页的 ETag/大小/修改时间核对，不逐个 HEAD、COPY 或 GET；只上传并完整读回新增或变化内容。每版保留完整逻辑文件清单和显式源对象映射，物理对象不再完整复制。源对象变化、缺失或分页预算不足会拒绝发布。清理器保护仍被清单引用的源版本；零新增对象的逻辑版本也按收据退休，退休后解析入口拒绝访问。源保护可能比复制型版本更保守，不保证立即回收所有未引用的单个对象。Git 恢复按原清单恢复实际来源对象，保持来源 MIME，不改写逻辑引用。

本地未变内容跨版本共享只读 inode，变化文件只创建新 inode，禁止原地编辑版本目录。完整路径集合、类型、大小、只读模式及验证凭据在切换前后核对；新增字节始终做完整 SHA-256、SVG/LFS 与容量验证。凭据过期、内容或元数据变化时退回完整验证。空闲 Worker 以两秒预算后台重新计算内容摘要，推送优先；凭据不因重复复用而延长完整校验时间，一小时过期后不能继续盲用。显式图片回退和对象恢复仍执行完整字节验证。共享 inode 无法提供多份物理副本的故障隔离，必须保留 Git、S3 与独立备份，目录逻辑占用不代表可回收磁盘空间。

升级前备份程序、私有配置、路由及旧镜像；先部署支持引用读取/清理/恢复的程序和健康的 `object-reader`，保留原 `objects` 服务供其他路由使用，只切换版本路由，再启用配置。新增来源对象只有完成验证后才写完整清单，最后原子切换图片。不可将引用格式直接交给旧版清理器。

要关闭优化，可将 `reference_releases` 改为 false 并重建 Publisher，后续使用完整物理版本；已有引用 URL 仍需要读取组件及引用保护。回退旧 Publisher 二进制时须移除旧版本不认识的字段并弃用增量凭据，同时保留新读取/清理/恢复兼容层。完全移除兼容层前应先恢复并物化所有保留的版本对象，不能直接切回原始 S3 路由。

## 即时队列与单目录图片快照

Linux Worker 在扫描前注册 jobs 目录的 inotify，原子入队后即时唤醒；通知仅是提示，真实任务仍先 fsync。启动先扫队列，通知遗漏、队列溢出或平台不支持时仍定期扫描，不依赖通知保存任务。失败保留既有退避和心跳。

引用发布的新清单标记 `image_layout: snapshot-v1`，`current` 直接原子指向 `releases/<sha>/images`。不再另建 `images-<sha>` 目录，因此不再为同一版本重复遍历及建立图片硬链接。Nginx 的公开路径和挂载不变；目录内不允许符号链接，切换前后仍核对完整路径集合、类型、大小和只读 inode 凭据，显式回退仍读全量字节。旧目录格式继续支持，清理与恢复识别两种格式。第二次分支检查用受限 `ls-remote` 获取精确 ref，保持发现新 Push 时拒绝激活的行为，不再为无变化的最终检查重新 fetch 和扫描对象预算。

`open-assets deduplicate-local` 默认仅预览；加 `--apply` 才执行。持有 Worker 锁并拒绝有待处理任务时执行，先完整核对所有已有清单中的本地文件字节，再将同仓库相同 SHA-256/大小的文件原子替换为共享只读硬链接，不删逻辑版本、清单或不同内容。未列入清单的数据不处理；异常或校验冲突中止。已完成的全量字节校验和受控硬链接替换可以重建当前版本的本地 inode 凭据，同时保留原来完整读回验证过的 S3 ETag、修改时间和 Git OID 来源；缺少或不匹配的来源凭据仍失效并退回完整发布。不会凭空信任任意远端元数据，也不会为了预热再上传一份相同图片。以实际 `du` 核对物理占用；目录逻辑大小不能用于判断重复存储。硬链接 inode 的 mtime 相同，但内容与 URL 保持；Git、S3 和独立备份仍是不同的恢复来源，不能为了“只存一份”一并删除。

回退旧程序前，先运行当前版本 `open-assets materialize-image-views --apply`，完整校验并为保留的新格式版本重新建立旧 `images-<sha>` 硬链接视图，再将 current 切回旧格式；这只增加目录项，不复制图片数据。继续保留引用对象读取、清理与恢复兼容层。禁止直接交给不认识新激活路径的旧清理器。

历史 S3 对象可用 `compact-objects` 单次去重：仅处理已使用引用发布的仓库，先并发完整 SHA-256/MIME 验证所有清单来源，保留当前版本的来源选择，再持 Worker 锁和读取端共享屏障把旧清单原子改成直接来源引用；全部清单落盘后才分批删除已不被任何清单使用的重复物理对象。由旧完整版本迁入的清单保留原来未列入清单的手工对象 URL 透传；此类额外对象不删除。不同字节、不在清单中的对象和非引用仓库不处理，版本化桶拒绝执行。默认预览，`--apply` 才执行；元数据备份和逐批记录位于 state/object-compaction，部分删除失败会停止并保留恢复材料，不声称去重完成。元数据备份不能直接恢复成旧清单，否则会指向已移除的重复对象；应使用保留的规范来源或先运行 Git 恢复。

先升级并重建 `object-reader`，准备固定 `object-retention/compaction.lock`（不可替换 inode）。读取端从清单解析到响应结束持共享锁；合并端取得独占锁、等待原请求退出后提交全部清单，再释放读取屏障，随后删除重复键，因此正常请求不需要等待整个对象删除过程。应用前核对读取端 guard-v1 健康标记。根权限运维可运行 `bin/open-assets operator objects setup --compaction` 临时授予独立清理身份仅在已配置仓库命名空间内的 DeleteObject 权限，再以 object-cleaner 服务执行单次合并，结束后务必再运行 `objects setup` 撤销临时删除权限；不赋予 Publisher 删除权限、不启用自动清理，也不改保留策略。必须维持版本命名空间的独占不可变写入约定。

已同步文件数据的快照按目录深度自底向上同步，最多四个并发目录任务、有限待执行队列；父目录等待全部子目录完成后才同步，任何 fsync 错误都拒绝激活。目标版本目录在切换 current 前也必须同步。保留完整切换前后校验，去掉额外的第三次部分扫描；任务耗时另外记录 evidence 阶段，并将凭据落盘计入完成时间。

对象来源核对按每组最多 1000 个已知键划分有上下界的范围，用 StartAfter 与分页在有限并发下核对。每个被引用对象仍必须出现且 ETag、大小和修改时间一致；额外键不会掩盖缺失的边界对象，全部范围共享 128 页预算。任何失败都发生在新对象写入和激活之前，不用短时缓存代替核对。

Git 拉取仍写独立临时 ref 并执行完整的拉取前/中/后预算检查；有界、无符号链接的临时 ref 读取替代正常路径的额外 rev-parse，随后一次 update-ref --stdin 原子提交 source 更新和临时 ref 删除。失败仍清理临时 ref，不提前提升未经预算检查的提交；Git 进程和输出限制保持。

Worker 可复用最长 30 秒的 S3 HTTP 连接池；每次请求仍重新签名，每次发布仍核对全部引用对象。端点、桶、密钥文件或 CA 的 inode/大小/时间变化立即关闭旧池并重建，不缓存鉴权或核对结果。缓存仅属于单个 Worker，退出时关闭；独立 CLI 不受影响。

Git 增量导出使用不带大小查询的完整 ls-tree 枚举，仍核对每个条目的路径、模式和发布预算。只有与有效本地不可变字节凭据对应的 Git OID 才复用已验证大小；新增 OID 分批执行 cat-file --batch-check（每批最多 1024 个），严格匹配响应顺序、类型和大小。凭据不可用时保留完整大小查询与字节校验。

完整导出计划通过路径、类型及大小限制后，只读来源对象核对可与本地快照准备并行。核对结果仅用于同一发布、完全一致的复用计划；上传任何新对象前必须等待核对成功。计划不一致、核对失败或导出失败均不激活，退出时等待有限后台任务结束，不缓存核对结果。

来源列表的标准 UTC LastModified 时间使用严格格式匹配和 datetime.fromisoformat 解析，保留微秒及 UTC 语义；其他格式或无效日期回到 botocore 原解析器。只改变应用独立 SDK 会话的响应解析工厂，不改全局 SDK、不缓存核对结果，不减少任何对象的 ETag、大小或修改时间检查。

本地不可变快照按源 inode 分组，单个 inode 的所有别名串行链接，不同 inode 以 object_workers 指定的上限并发。待执行批次有界，统计由主线程合并，清单仍按原 Git 顺序生成。每个链接仍核对操作前后的身份、只读位和 nlink；SVG 验证、完整激活前后校验及目录同步均保留。异常取消未启动任务并等待运行中任务退出，之后才清理暂存目录。父目录创建使用最多 4096 项的有界缓存，重复目录仍拒绝符号链接。

Git 拉取的缓存预算在开始前及提升临时 ref 前强制完整核对；运行中的重复扫描合并到约 100 ms 一次，保留进程时间/内存/文件大小限制及最终拒绝超额提交的边界。这些是准入预算，仍不是文件系统硬配额。元数据 JSON 使用一次编码和写入减少 Python 逐片编码开销，文件 fsync、原子替换、父目录 fsync 的顺序及记录格式不变。

引用模式的空闲 Worker 可以预先准备一个未对外服务的完整硬链接视图。准备前后核对当前完整树，视图及父目录全部同步后才标记 ready；只更新由本次已校验链接造成的 ctime，不延长完整字节校验时间。视图只使用硬链接，不能链接时禁用该可选优化，不复制图片。每个仓库最多一个检查点视图，失败或配置不匹配时保留有界检查点并走原发布路径，不无限重试或累积副本。空闲准备有计算时间预算，已到达的发布任务优先于开始下一次准备。

下一次发布仍验证 Git 完整计划、全部待用视图文件的身份/只读字节凭据、SVG 和全部远端来源对象。只在同一发布基线、配置和清单摘要下消费视图；变更及删除只作用于未发布视图，解除硬链接同样核对前后身份和 nlink。已同步的不变目录可保留，只同步本轮变化目录及其全部祖先；新字节先 fsync，子目录先于父目录同步，激活前后仍检查完整服务树。旧 Publisher 可忽略该可选视图，沿用原 v3 凭据和完整发布/回退流程；损坏或过期的视图会退回现有校验流程。

待用视图与发布基线不再一致时，只回收检查点登记的私有前缀目录；使用防符号链接攻击的目录回收，核对共享 inode 的 nlink 减量和回收前后的完整当前树，再刷新仅由回收造成的 ctime，不延长字节校验时间。已有生产凭据证明的同一 inode 可用于刷新待用视图的别名凭据；实际文件身份仍必须匹配。这样回退、正常链接清理及一次缓存回退后仍可继续准备新视图，不累积无用视图。

### Git control round trips

Fetch returns the exact commit whose temporary ref passed the resource checks and
was promoted atomically. Publication uses that value instead of spawning another
`rev-parse`; equal commit identities need no ancestry subprocess. Manual publication
of a different commit still checks ancestry, and automatic activation still reads
the authenticated remote branch again. Fetch does not write the unused FETCH_HEAD.
Git child completion wakes the bounded monitor promptly instead of sleeping a full
poll interval after an already completed command. Time, output, memory and disk
budgets and process-group cleanup remain in force.

SSH connection reuse avoids repeated handshakes, but each command still has channel
opening, remote authorization and Git advertisement/report-status exchanges. Measure
real client push-to-published time separately from server job time; a warm SSH master
alone does not imply a three-second end-to-end publication.
