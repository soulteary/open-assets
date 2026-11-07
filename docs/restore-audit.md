# 恢复审计查询与归档

这组命令管理 `close-restore` 产生的审计目录，包括成功结案、拒绝、失败和未完成的尝试。查询合并在线记录与已验证的归档；归档默认只预览，必须显式添加 `--apply` 才移出在线目录。它不归档原 `object-restores` / `restore-checks` 报告，不修改对象、图片、清单、current/published 或清理屏障。

## 查询

在 `/app/open-assets` 中执行：

```sh
# 指定仓库；在线与归档合并，默认返回最近 100 条。
sudo docker compose exec -T publisher python -m open_assets \
  restore-audit-list "$ASSET_KEY"

# 指定完整 SHA，并使用分页。
sudo docker compose exec -T publisher python -m open_assets \
  restore-audit-list "$ASSET_KEY" "$ASSET_COMMIT" --limit 100 --offset 0
```

JSON 含 total、has_more、offset 和 records。记录列出审计 ID、完整 SHA、状态、开始/结束时间、有限原因、逻辑字节及 locations：online、archive、retired。retired 是已完成归档校验但清理尚未完成的暂存目录。同一记录在多个位置存在时合并显示，避免因中断重复计数。

查询不显示自由文本操作原因、UID、原始报告或凭据；完整证据保留在私有文件/归档中。输出的 SHA 与审计 ID 也属于运行材料，不提交 Git。仓库必须仍在配置中，已禁用仓库可以查询；完全移出配置的仓库应先恢复其登记，再操作审计。

## 默认预览

```sh
sudo docker compose exec -T publisher python -m open_assets \
  restore-audit-archive "$ASSET_KEY"
```

默认保留每仓库最近 20 条在线记录，完成不足 30 天的不归档，每轮最多 100 条。只预览候选、保护原因、逻辑字节及归档后的预计在线数量，不创建归档或暂存目录。预览与实际执行都使用 Worker 维护锁；锁忙时返回 busy，不等待发布结束。

可按 SHA 缩小候选，或显式设置保留窗口：

```sh
sudo docker compose exec -T publisher python -m open_assets \
  restore-audit-archive "$ASSET_KEY" "$ASSET_COMMIT" \
  --keep-last 20 --min-age-days 30 --max-records 100
```

keep-last 为 1–1000，不能为零；min-age-days 为 0–36500；max-records 为 1–500。keep-last 始终按整个仓库计算，SHA 过滤不能绕过仓库最新记录的保护。候选按最早启动时间优先处理；年龄按 finished_at 计算。字节是原文件的逻辑长度，不是实际磁盘回收量或压缩包大小。

以下记录不移出在线目录：

- intent 存在但 result 缺失的未结案尝试，包括仍在执行或中断后未完成的目录。
- 文件缺失、损坏、链接、未知条目或证据绑定不符的目录。
- 最近保留数量内的记录，至少保留最新一条供 Info 展示。
- 完成时间未超过保留窗口的记录。
- 引用了不确定原报告的 refused/failed 尝试，且同一原报告摘要没有可验证的 closed 记录覆盖。覆盖证明可以来自在线记录或已校验归档。

普通已完成的 refused/failed 可以归档；归档不将它们改成成功。原报告在成功结案后仍保留不确定字段是正常的，closed 审计本身已经确认该报告，所以可按保留策略归档。先前失败涉及的另一份未结案证据没有被覆盖时，仍继续保护；不能用新 SHA 或新的恢复报告替代它。

预览给出 protected_total、protection_counts、invalid_records。保护明细最多返回 1000 条，超出时 protected_truncated 为 true；候选受 max-records 限制。若预计在线数量仍高于 Info 的 512 上限，应检查保护原因，不要通过删除未结案记录隐藏问题。

## 显式归档

确认预览后执行相同命令并添加 --apply：

```sh
sudo docker compose exec -T publisher python -m open_assets \
  restore-audit-archive "$ASSET_KEY" \
  --keep-last 20 --min-age-days 30 --max-records 100 --apply
```

执行时重新扫描和计算保护范围，不直接执行旧预览。每条记录按以下顺序处理：

1. 读取完整目录，验证 intent/result、快照摘要及成功结案的核对绑定关系。
2. 生成私有 tar.gz，保存全部原始文件字节及 manifest.json。清单包含仓库、SHA、审计 ID、归档时间、执行 UID，以及每个文件的长度和 SHA-256。
3. 刷盘并以不覆盖方式发布压缩包，重新读取、限额解压校验，逐文件核对字节及审计语义，再确认在线源未变化。
4. 在同一 state_root 文件系统内原子移动在线目录至 retired 暂存目录。此时归档已经完整验证。
5. 再次核对归档及暂存剩余文件，只删除与归档逐字节一致的暂存内容。归档包保留，最近在线记录不动。

```text
state_root/
├── restore-closures/<key>/<audit-id>/       # 在线目录
├── restore-audit-archives/<key>/<audit-id>.tar.gz
└── restore-audit-retired/<key>/<audit-id>/  # 中断清理可重试
```

压缩包及目录为私有权限，文件 0600、归档目录 0700。归档保存原文件字节和 JSON 中的时间，不保存原文件系统 UID、mtime 等属性；包内文件使用固定的权限/时间元数据。tar.gz 不加密，也不是数字签名或管理员不可篡改存储，需与原审计一起备份到受控的独立存储。

## 验证归档及中断恢复

```sh
sudo docker compose exec -T publisher python -m open_assets \
  restore-audit-verify "$ASSET_KEY" "$ASSET_COMMIT"
```

verify 校验所选仓库的压缩包和待清理暂存副本，不解压到文件系统。它检查成员白名单、类型、重复名称、长度、SHA-256、身份和结案绑定，拒绝链接、路径逃逸、异常尾部、截断及展开超限。每包压缩数据最多 256 KiB，展开最多 128 KiB；原 intent/result 各最多 8 KiB，source/check 各最多 16 KiB。

已发布压缩包但在线目录未移动时，重试验证并复用同一包，不覆盖它。目录已移动或暂存只删了一部分时，重试验证归档和所有剩余字节后继续；即使 intent 已从暂存移除，也必须以完整、有效的归档为依据。归档损坏、同名内容冲突或剩余文件不符时停止，保留现场，不自动替换/丢弃。使用相同参数重新预览，再显式 --apply；不要手工清空 retired。

一次 apply 不是整批事务。失败时 archived_records 仅统计已完成处理的条目，partial_possible 表示已经可能生成包、移动目录或清理部分暂存。不能据此推断其他条目无变化。查询/verify 可确认实际位置；最终结果丢失时也应先检查，不能直接删除原目录。

## 上限、退出码和 Info

每命令只处理一个仓库，扫描该仓库在线、归档和暂存位置，合计最多 20000 个目录项，时间预算 30 秒。limit 为 1–1000，offset 为 0–1000000，仅控制查询/verify 输出分页，不跳过完整性检查。达到扫描上限或遇到未知布局会停止，不以不完整列表做删除决策。应用锁不阻止外部管理员并发写入；操作期间不要绕过应用修改目标。

退出码：0 表示扫描/规划/执行完成；1 为校验错误或执行失败；2 为锁忙或参数用法错误。预览/apply 的 complete 不表示没有受保护或坏记录，要同时检查保护计数、invalid_records、预计在线数量；list/verify 发现所选范围内或无法判定 SHA 的坏归档时返回 failed。归档不会自动定时执行，也不会清理归档包。

Info 继续只读在线目录，不在每次采样时遍历压缩包。448 条开始提示预览归档，超过 512 条时提示读取超限。显式归档后，在下次正常主机采样时恢复显示；最新在线记录仍保留。若超限主要由未结案/损坏记录造成，需要先复核并处理，归档 CLI 不会强制移走它们。原恢复与核对报告的独立读取上限不由本命令处理。

归档与查询均不拉取 Git，也不重新读取 S3 对象；verify 验证的是审计包，不是业务内容的当前完整性。归档默认仍在同一数据盘，不替代独立备份。强制终止可能留下私有 tmp 临时文件，CLI 不自动删除无法关联的临时内容；确认没有维护进程后再人工核对。

显式 --apply 现在另存每仓库一份有界的[实际归档观测摘要](info.md#审计容量缓存与实际归档结果)，用于 Info 展示最近结果、参数和保护原因；默认预览、list、verify 仍不写状态。摘要与归档文件不是联合事务，硬终止或结果写入失败可能留下 running；不能仅凭摘要判断没有生成包或移动目录。Info 的容量采用独立 15 分钟缓存，默认保护规划与最近手工参数分开显示；实际归档不会自动触发高频统计。
