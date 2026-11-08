# OtterIO 历史对象保留与清理

## 默认行为与范围

对象清理与[本地版本清理](retention.md)独立。object_retention.enabled 默认为 false，命令默认只预览；示例定时服务也不带 --apply。启用本地 retention 不会启用对象删除。预览会获取锁、读取列表并保存最近结果，但不删除对象或修改版本内容。

仅处理配置桶内 repositories/<key>/releases/<完整 SHA>/ 下、可与本地可信清单对应的对象。不会删除桶、Git、图片目录、清单、任务、备份、身份数据、未知前缀或未完成分片。版本名必须是非零的 40 位小写十六进制 SHA。

对象删除是永久操作，旧附件 URL 会失效；客户端已有缓存不代表源对象仍可恢复。上线实际删除前，应备份对象及元数据，并确认历史链接的保留要求。空间不足不会自动突破保护规则。该功能不是存储配额，也不保证释放的物理字节数等于对象逻辑大小。

## 配置

在私有 repositories.yaml 顶层配置默认值，每仓库可覆盖单项：

```yaml
object_retention:
  enabled: false
  exclusive_writer: false
  keep_last: 10
  keep_days: 90
  min_age_hours: 24
  pinned_shas: []
  max_delete_versions: 2
  max_delete_objects: 100
  max_delete_bytes: 268435456
repositories:
  - key: main-images
    # 保留已有身份、源目录、分支和挂载配置；此处不是完整配置。
    object_retention:
      enabled: false
      keep_days: 180
      pinned_shas:
        - 0123456789abcdef0123456789abcdef01234567
```

exclusive_writer 默认为 false。只有确认该前缀由本应用独占写入、执行期间不通过 OC/其他 S3 客户端或并行运维程序修改对象和引用时，才可显式设为 true。当前 OtterIO 不提供可靠的 If-Match 删除条件；无法满足这一前提的仓库只能预览。

keep_last 至少为 1；keep_days 可以为 0；min_age_hours 至少为 1。每仓库每轮最多删除 max_delete_versions 个版本、max_delete_objects 个对象和 max_delete_bytes 个逻辑字节，必须同时满足；超过单轮预算的整版会跳过，不把它拆成正常的多轮删除。失败中断后的剩余对象可以凭收据恢复处理。

修改配置前备份并执行 validate；原子替换单文件 bind mount 后按部署流程重建 Publisher/Webhook。独立清理容器每次创建时读取最新配置。新增白名单仓库 CLI 保留已有 object_retention 设置；新增 key 或启用、禁用某仓库删除后，重新执行下述 objects setup，使 IAM 同步。后续新建的清理进程会按禁用策略停止删除，重新 setup 则撤销存储端旧授权。配置不会撤销正在执行的旧进程；调整前先暂停定时器并等待当前作业退出。

## 保护集合与候选判定

以下条件取并集，不承诺最终只剩 N 个版本：

- current、与其一致的 published 记录及 rollback-protection 中的 SHA；回滚历史长度仍由本地 retention.rollback_versions 配置。
- 本地 retention.pinned_shas 和对象 object_retention.pinned_shas 的并集。
- pending、running、images_prepared、failed 任务的 SHA，以及这些任务和当前版本任务中的 previous 引用。
- 按近期使用时间排序的最近 keep_last 个对象版本；keep_days 或 min_age_hours 尚未到期的版本。
- 缺少本地清单、清单对象集合不完整、存在额外对象或大小不匹配的版本。无 current 时，所有发现的版本保留。

排序和年龄使用对象 LastModified、本地清单及版本目录更新时间、有效任务创建/完成时间的较新值，不使用 Git 作者时间。重新发布、回滚或修改对象可能延长保留窗口。本地版本即使已清理，只要清单与保护引用仍完整，对象仍可单独规划。

损坏的任务、current/published 不一致、无效清单、链接元数据、冲突的删除收据、列表超限或超时会阻止清理。孤立或未知对象只报告数量，不自动认领删除；请先调查来源，不能用清理程序代替修复。

## 独立账号与初始化

先更新应用源码并构建镜像。正常部署仍只运行原有常驻组件；object-cleaner 位于 maintenance profile，每次执行后退出，不提供公网服务。

```sh
cd /app/open-assets
sudo scripts/open-assets objects setup
# 可重复执行：保留账号、凭据和 Publisher 策略，只收敛专用清理策略。
sudo scripts/open-assets objects setup
```

setup 使用宿主机已有 OC 管理配置创建专用身份，生成私有 object-cleaner 凭据目录。只授予已配置仓库 releases 前缀的 ListBucket/GetObject 和桶 GetBucketVersioning；DeleteObject 仅授予 object_retention.enabled=true 且 exclusive_writer=true 的仓库前缀。默认配置下没有删除权限。不授予 PutObject、DeleteObjectVersion、桶删除、版本控制修改、生命周期修改或保留锁绕过权限。Publisher 原有账号及策略保持独立，不新增删除权限。

清理容器以非 root 运行、只读根文件系统、单独的存储网络、只读凭据与图片挂载，只有发布状态目录可写，用于互斥锁和删除收据；不挂载 Docker socket。默认限制 256 MiB、0.5 CPU 和 64 个进程。它与发布共享状态和锁，因此不是对已失陷 Publisher 的完整隔离边界；宿主机管理账号仍属于受信任运维范围。

## 预览与执行

```sh
# 默认只预览，输出保护原因、候选及逻辑字节数。
sudo docker compose run --rm --no-deps object-cleaner --key main-images

# 省略 key 检查全部已配置仓库。
sudo docker compose run --rm --no-deps object-cleaner

# 需先设置目标仓库 enabled=true 和 exclusive_writer=true，再同步 IAM。
sudo scripts/open-assets objects setup
# 同时具备两个配置开关、IAM 删除授权和 --apply 才会删除。
sudo docker compose run --rm --no-deps object-cleaner --key main-images --apply
```

执行时重新计算保护和候选，不接受保存的预览作为删除授权。最好先对一个仓库启用、预览并核对备份，再手工执行。策略已启用但 IAM 尚未同步时，删除会失败，不会退回 root 凭据。

最近完整报告保存在 state_root/object-retention-last.json，包含 apply、候选、保护原因、实际删除数量与逻辑字节、完成时间。CLI 的 busy 表示未获取到锁，本轮没有执行；blocked 且非零退出表示遇到不安全状态或错误。失败前可能已删除部分对象，不能把非零退出理解为事务回滚；旧的完整报告只代表其时间点，失败详情需结合本次输出、journal 和收据核对。

## 校验、条件删除和中断恢复

清理持有独立清理锁，以及与发布、回滚和入队共用的非阻塞 Worker/Queue 锁。保护读取至删除期间引用不由正常应用流程变更；持锁忙碌则跳过，不等待或移除他人锁。规划完所有目标仓库后才开始删除。预览也会短暂阻塞发布，建议在低峰使用定时器，不要频繁启动。

每轮共享 45 秒检查预算、500 次 S3 请求和 50,000 个列表条目计数，重复核对的列表条目也计入预算。每页最多 1000 个对象，请求起点间隔至少 0.1 秒；连接/读取超时 2/5 秒，关闭 SDK 自动重试。预算不能中断已开始的网络调用，示例 systemd 另设 120 秒上限。过大的仓库会停止，不能通过删除缓存绕过限制；需要分仓、保留明确范围或设计专门离线作业。

对每个候选重新列出完整版本前缀；删除前对整版对象逐一 HEAD，要求大小、ETag、时间及 sha256 元数据与列表/可信清单一致。这是元数据围栏，不会重新下载并计算完整内容哈希。随后先刷盘写入 deleting 收据，再逐对象删除，最后确认前缀为空。请求仍附带 IfMatch ETag，但当前 OtterIO 实测会忽略该条件，程序不依赖它提供原子保证；安全前提是显式的 exclusive_writer 和应用共享锁。对象被外部修改时，HEAD 与 DELETE 之间仍有竞态，不能宣称所有外部写入都能被阻止。[S3 DeleteObject 语义](https://docs.aws.amazon.com/AmazonS3/latest/API/API_DeleteObject.html)

仅支持未启用过版本控制的桶。Enabled 或 Suspended 状态均停止，不产生删除标记来冒充释放空间；不删除非当前对象版本，也不绕过对象保留锁。独立实例实测错误 ETag 仍可能删除对象，因此没有把条件删除作为当前 OtterIO 的安全保证。更换实现或版本后仍应重新验证，不能仅凭 SDK 接受参数就认为服务端支持。

S3 不支持整版原子删除。网络失败、超时或进程停止可能留下部分已删除版本；收据保留原对象集合、ETag、时间与清单摘要，并绑定桶/endpoint 范围。重试重新检查当前保护，只允许处理原集合中仍存在且未变化的对象。新增对象、替换内容、清单改变或范围改变会停止，不扩大删除范围。即使已经删完但尚未记录完成，也能在下一轮将有效收据结案。

已完成收据压缩为不含对象列表的完成标记，不授权未来同 SHA 的新对象删除。实际执行的发布或对象恢复会先恢复并核验 S3 内容，再写 restored 标记；对象已删或正在删的版本不能直接 rollback-images，以免图片回滚看似成功而附件已失效。恢复已清理对象应使用[restore-objects](object-restore.md)，或从可信备份恢复后进行等价核验；普通 publish 可能命中终态任务去重，不保证重新上传。不要手工删除收据绕过检查。

每仓库最多检查 10,000 份收据，超限停止；这些记录、清单及任务归档不由本功能自动清理。收据缺失会失去这部分回滚阻止依据，应与状态一同备份。受信任运维人员绕过锁改对象、引用、桶版本控制或 IAM 仍存在竞态；执行清理期间应避免这类并行管理操作。

## 定时预览与暂停

```sh
sudo install -m 0644 config/systemd/open-assets-object-retention.service config/systemd/open-assets-object-retention.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now open-assets-object-retention.timer
sudo systemctl start open-assets-object-retention.service
sudo journalctl -u open-assets-object-retention.service --since '1 day ago' --no-pager
# 暂停后续计划，不会回滚已经删除的内容。
sudo systemctl stop open-assets-object-retention.timer
```

示例每 6 小时预览，并有最多 15 分钟随机延迟；开机 25 分钟后可首次触发。默认服务不含 --apply，即使仓库策略已启用也不会删除。自动应用需由运维显式创建 systemd drop-in，清空 ExecStart 后重设为带 --apply 的同一命令；仍受每仓库策略、IAM、保护集合及预算约束。不要直接覆盖源代码中的默认服务来启用删除。

对象统计缓存不会因每次预览而失效，实际清理也不强制重新遍历。Info 中对象数量与逻辑大小会在下一次完整对象采集后更新；本地“最近清理”仍只表示本地清理，不能当作对象删除报告。

重新发布仍遵守原有任务去重：publish 返回已完成或已归档的旧任务 ID，并不表示发生了对象恢复。已清理对象请使用[restore-objects](object-restore.md)，它从原 Git 提交和可信清单同步恢复，不依赖任务重放，也不切换当前图片版本。不要把已完成任务改为 pending 或删除归档来绕过去重。

## 看板摘要与最近尝试

每次有效调用会记录各目标仓库的开始状态，完成后分别保留 last_plan 和对应模式的 last_preview/last_apply；失败、忙碌跳过与未结案执行不会替换完整历史。新摘要位于 state_root/object-retention-results/<key>.json，只含标量计数、固定原因码、策略摘要和时间，不保存对象列表或完整 SHA。旧的全局详细报告和删除收据仍各有用途，不能混为同一类记录。

摘要写入失败会阻止尚未开始的清理；若在对象删除之后写入失败，可能留下旧完整结果或未结案状态，应按收据检查实际影响。执行中的进度在内存中累计，异常退出时记录已收到删除响应的数量；进程强制退出或响应丢失时只能提示可能部分删除，无法保证精确计数。看板的候选总量与预算内候选均为规划时数据，实际执行记录保留删除前规划；下一次预览才会重新计算剩余候选。详见[看板字段与边界](info.md#otterio-对象统计)。
