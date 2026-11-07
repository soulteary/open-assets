# 发布流程与状态

## 内容从哪里来

接收器只接受 `POST /hooks/publish-assets`，请求体上限为 1 MiB。它对原始请求体验证 Gitea HMAC-SHA256，再检查 Push 事件、仓库 ID、配置分支和提交 SHA。请求中的 clone URL、Shell 命令或磁盘路径不参与执行。

仓库 ID 用于匹配白名单，稳定 key 用于本地目录、任务身份和 S3 前缀。仓库改名不会自动修改配置中的固定 SSH remote，需要管理员明确迁移。

接收器将任务写入临时文件，刷盘后重命名并刷新父目录，再返回 202。该响应表示入队完成。磁盘故障、文件系统或硬件的实际持久化能力仍取决于部署环境。

## 去重和执行顺序

任务 ID 由 `key + SHA + kind` 计算。delivery ID 只作为记录，不作为任务身份。同一个 SHA 的自动任务不会因为重复投递而新增；失败的同类任务也不会被新的 Push 自动改回 pending。

队列锁保护入队，Worker 锁保护发布和回滚。两把锁分开，大文件发布时仍可接收新任务。Worker 每轮逐个读取和校验任务，把无效记录保留到 quarantine，再按创建时间处理有效待执行记录；这是单机文件锁方案，不支持多个节点共享队列。

自动任务只发布当前远端配置分支头。Worker 拉取时检查一次，激活图片前再次拉取检查；旧任务不会在这两个检查点之后被当作最新任务处理。但远端在第二次检查后仍可能再次收到 Push，因此这不是与 Git 分支头绑定的原子事务。

人工 `publish` 允许配置分支历史中的指定提交，不要求它是最新分支头。用历史 SHA 执行人工发布会改变当前图片版本，请按回滚操作对待。

## 一次发布做什么

1. 从配置的 SSH remote 拉取分支，检查提交资格。
2. 从 Git 对象库导出指定源目录，不 checkout，也不执行仓库脚本。
3. 拒绝特殊 Git 条目、隐藏路径、LFS 指针和超过限制的文件，计算 SHA-256 清单。
4. 把完整 public 内容上传到 `attachments/repositories/<key>/releases/<sha>/`。同名对象已存在时先比对内容，内容不一致则失败。
5. 下载对象验证字节，为该提交保存本地清单和完整文件副本。
6. 单独准备 images 子目录，完整写入后才使图片版本目录生效。
7. 再检查自动任务是否过时，记录切换前版本，原子替换该仓库 current 软链接。
8. 图片激活前后检查实际图片目录的文件集合、类型、大小和 SHA-256；切换或校验失败时尝试恢复原链接，成功后写入当前发布记录和任务状态。

代码使用文件和目录 fsync 以及同文件系统 rename，减少进程中断留下不完整当前目录的可能。文件系统、对象存储和 Git 服务之间没有联合提交。

## 状态和重启

持久化任务使用 `pending`、`running`、`images_prepared`、`published`、`failed` 和 `superseded`。对象上传进度、逐文件完成情况没有单独持久化状态。

Worker 重启会重做 pending、running 和 images_prepared。已有对象和本地版本会再次校验；远端 main 已前进的自动任务会转为 superseded。failed 需要人工 retry，published 不重新执行。同一 SHA 的人工与自动任务属于不同 kind。

失败可能留下已经上传的对象或临时目录；项目没有自动清理或补偿删除。失败状态不表示远端没有发生任何写入。

## 文件布局

宿主机上的主要路径：

```text
/data/asset-stack/publisher/
├── git/<key>/                    # 裸仓库缓存
├── jobs/<job-id>.json             # 任务状态
├── job-archive/<prefix>/<id>.json # 保留去重身份的终态任务
├── rollback-protection/<key>.json # 最近回滚点
├── retention-results/<key>.json   # 每仓库最近完整清理计数摘要
├── quarantine/<id>/             # 原始坏记录和隔离原因
├── manifests/<key>/<sha>.json     # 完整文件清单
└── published/<key>               # 最近激活版本的记录

/data/asset-stack/images/repos/<key>/
├── releases/<sha>/               # public 的完整副本
├── releases/images-<sha>/        # Nginx 实际使用的图片副本
└── current -> releases/images-<sha>
```

以实际 `state_root`、`images_root` 及 Compose 挂载为准。图片版本在本地保留完整 public 与图片子集，估算容量时应包含这些重复数据。

## 读取边界

Nginx 只读挂载 images 父目录，按仓库前缀读取 current。图片切换不需要 reload；新增 URL 映射才需要生成配置、校验并 reload。

附件入口只转发 GET/HEAD 和固定桶前缀，OtterIO 另用桶策略限制匿名读取。公开 URL 指向的内容本身就是公开数据，不能把不想公开的文件放进 public。OC 下载地址依赖管理会话，不等同于公开附件链接。

图片回滚先校验清单身份及完整 public 快照，再通过同一图片激活函数校验并切换实际图片副本。不自动重建损坏目录，具体操作与隔离记录处理见 [运行维护](operations.md)。

本地历史保留按数量、时间和引用集合判断，只清理未受保护的版本，终态任务可归档但保留去重身份，OSS 对象继续保留。实际删除前先把目录移到仓库内 `.retention-trash/`，中断恢复只处理已认领目录；完整规则与定时运行见 [历史清理](retention.md)。

对象容量由独立 open-assets-objects.timer 低频触发，在 Publisher 内通过现有 S3 身份分页列表；宿主机缓存完整结果和续采进度。30 秒主机采集只读取缓存白名单字段，Info HTTP 仍只读最终状态快照，不访问 S3。

对象清理由 maintenance profile 的短时 object-cleaner 执行，复用 Worker/Queue 锁和保护引用，使用专用 S3 身份及只读图片挂载。默认定时预览，删除需独立显式启用；收据保存在发布状态下 object-retention/<key>/<sha>.json。已删对象版本的本地回滚被阻止，正常重新发布核验对象后写 restored 标记。详见[对象保留](object-retention.md)。

对象清理进程另写每仓库 object-retention-results 摘要，独立保留完整预览与完整执行。主机采集只读这些摘要和当前策略，输出固定原因码、计数与时间；对象规划仍由低频维护任务完成，不进入 HTTP 请求或普通采集路径。

对象恢复是同步维护 CLI，持有 Worker 锁，从固定分支历史导出并与原清单匹配，补齐对象后完整读回复核；完成后更新恢复标记和清单使用时间。它不经过发布队列，也不改变任务身份、归档或当前入口。Worker 在维护锁忙时跳过该轮，维持心跳；恢复状态独立保存到 object-restores/<key>/<sha>.json。
