# 命令行使用

日常命令统一从 `sudo ./scripts/open-assets` 调用。先运行 `repo list` 查稳定 key，`doctor` 查配置/运行状态；发布、恢复、结案和审计命令可直接接在脚本后。每个子命令支持独立 --help，完整速查见[日常操作入口](operator.md)。下面说明仓库接入；日常发布与恢复使用同一宿主机入口。

## 仓库自动接入 CLI

此命令在部署服务器宿主机上执行，自动协调 Gitea 仓库、只读 Deploy Key、签名 Push Webhook、Publisher 的 S3 前缀权限、发布白名单和 Nginx 图片映射。不需要把 Docker socket 或管理员凭据挂到应用容器里。

## 接入命令

```sh
cd /app/open-assets
sudo ./scripts/open-assets repo ensure content/project-b \
  --key project-b \
  --mount-path /project-b/
```

如果已进入 root shell，省略 sudo。命令返回 JSON，`status: ready` 表示接入完成；它不代表仓库内容已经发布。首次创建的是空私有仓库，仍需使用个人 SSH 密钥提交 `public/images/` 和可选附件。

参数：

- `content/project-b`：Gitea 的 owner/repository。支持初始化账号 content 名下仓库，以及 content 有权限操作的现有组织。不会自动创建用户或组织。
- `--key project-b`：稳定发布身份，决定 S3 前缀和本地版本目录。绑定后不能用同一个 key 切换仓库。
- `--mount-path /project-b/`：图片 URL 前缀。只能有一个根 `/`，其他前缀不能重叠。新前缀如果遮盖根仓库当前已有目录，命令会拒绝。
- `--branch main`：可选，默认 main。命令不自动创建提交、改写分支历史或修改已有分支保护。
- `--root /app/open-assets`：全局选项，放在 repo 之前，用于指定部署根目录。该目录中的 Compose、private 与 .env 必须属于同一部署。

已有同名私有仓库可直接接入；公开或归档仓库不会被自动转换。已配置 disabled/serve=false 的项不会被命令重新启用。

## 执行前提

这是初始化之后的运维命令。需要 Linux 宿主机 Python 3、PyYAML、Docker Compose，以及完整的 private 配置：Gitea content 管理凭据、Publisher 公钥、Webhook Secret、已配置的 OC 管理 alias 与 CA、现有 assets-publish 策略和发布账号。

脚本只通过宿主机回环地址调用 Gitea API。密码从私有文件读取，不要求放在命令参数中。它不创建 root 存储账号，不重置现有密码，也不重新运行 initialize-services.py。

新增仓库或恢复中断执行时会短暂停止/重建 webhook 与 publisher；图片和附件服务继续运行。请安排发布维护窗口，看到 ready 后再 Push。窗口内旧仓库的失败 Hook 需重投或按 SHA 补发。

## 自动完成的步骤

1. 取得主机级锁，验证名称、稳定 key、URL 前缀和现有配置；前缀冲突在创建资源前拒绝。
2. 查询同名仓库，不存在时创建空私有仓库；将实际 ID 绑定到白名单。
3. 按公钥内容复用已有 Deploy Key，只创建缺失的只读授权。相同公钥若已有写权限则停止，要求管理员明确处理。
4. 按固定内部目标匹配 Webhook，创建或更新同一个 Gitea Push Hook。Secret 在读取 API 中可能被隐藏，因此重复执行会重写相同的私有 Secret，不新增 Hook。重复目标 Hook 会被拒绝，不自动删除。
5. 读取线上 assets-publish 策略，保留原语句和已有 Resource，只追加该 key 的对象前缀。确认发布账号已启用，缺少策略绑定时增量附加，不授予 consoleAdmin。
6. 更新发布白名单，生成候选 Nginx 配置，通过 nginx -t 后 reload。校验失败恢复旧 Nginx 文件，不 reload 无效配置。
7. 必要时重建接收器/Worker，确保 bind mount 使用新的配置 inode，写入私有接入记录并返回 ready。

## 重复执行与失败恢复

可以重复运行完全相同的命令。最终配置和资源不重复；已成功应用且配置未变时，`services_recreated` 为 false，不重建发布服务。命令仍核对远端状态、协调同值 Webhook Secret 和专用 IAM 策略。

绑定身份、仓库 ID、源目录、分支或 URL 前缀与已配置项不一致时停止；迁移不是 ensure 的隐式行为。分页查询覆盖已有 Deploy Key 和 Hook 列表。

多个系统之间不是事务。失败后已创建的仓库、Key、Hook 或新增 S3 权限可能保留，不会为了回滚删除用户资源。按错误阶段修复原因，再执行同一命令即可继续。即使上次停止在配置替换之后，也会根据接入记录与 inode 决定是否重建服务。

恢复材料在 `private/onboarding/<key>/`：初始 repositories 与线上策略副本、需要修改时保存的 Nginx 旧配置，以及成功后的 result.json。它们是私有材料，不提交 Git；不要未经核对直接恢复整份旧配置，以免覆盖后续其他仓库接入。

## 接入后发布

用个人 Gitea SSH 密钥克隆新仓库，按[首次发布](usage.md#首次发布空仓库)提交内容。第一次提交前不拉取不存在的远端 main 分支。若使用其他分支，将示例中的 main 替换为接入时的配置分支。

在服务器查询：

```sh
sudo ./scripts/open-assets status --key project-b
```

目标 SHA 为 `published` 后，使用 `links` 生成地址，详见[命令速查](operator.md)。已有内容需补发时，执行 `sudo ./scripts/open-assets publish project-b FULL_COMMIT_SHA`，然后继续查询任务结果。

## 对象清理账号

完成初始化后，执行 sudo scripts/open-assets objects setup 可创建或收敛专用清理身份。重复执行保留凭据，仅为显式启用且声明独占写入的仓库授予删除权限。仓库或开关变化后需重新执行；Publisher 不增加 DeleteObject。完整步骤见[对象保留](object-retention.md)。

## 恢复已清理对象

在部署服务器执行 `sudo ./scripts/open-assets restore-objects KEY FULL_SHA`，从固定 Git 分支历史重建并核验对象。它同步执行，不依赖任务重放；获取不到 Worker 锁时返回 busy 和退出码 2。命令及前提见[对象恢复](object-restore.md)。

本地副本也已清理时，添加 `--with-local` 重建完整 public 与图片副本，默认保持当前版本。确认需要切换后，使用 `--with-local --rollback` 在完整校验后显式回滚。已有副本冲突会停止，不强制覆盖；详见[恢复与切换](object-restore.md#完整恢复后显式切换)。

## 核对恢复状态

`sudo ./scripts/open-assets check-restore <仓库key> <完整SHA>` 只读核对对象、本地两份副本、current/published 与清理屏障，并保存独立诊断报告。默认不修复、不切换，也不结案原恢复。完整前提、结果与退出码见[恢复状态核对](restore-check.md)。

## 显式结案恢复

`sudo ./scripts/open-assets close-restore <仓库key> <完整SHA> --reason '人工复核原因'` 在同一维护锁内重新核对，通过后单独记录结案。原恢复必须未结案或状态不确定，且 current/published 必须存在并一致；不切换版本、不解除清理屏障、不改写原报告。前提、退出码和私有审计说明见[恢复结案](restore-close.md)。

## 查询与归档恢复审计

- `restore-audit-list <key> [完整SHA] --limit 100 --offset 0`：合并查询在线与已归档的结案审计。
- `restore-audit-archive <key> [完整SHA]`：默认仅预览，保留最近 20 条、完成不足 30 天及未结案记录；添加 `--apply` 才实际归档。
- `restore-audit-verify <key> [完整SHA]`：验证归档完整性和中断清理现场，不恢复或解包到在线目录。

均在部署服务器通过 `sudo ./scripts/open-assets` 执行。归档先保存完整文件与校验清单，验证后才移出在线目录；支持中断重试，不自动删除归档包。参数、保护范围、退出码见[恢复审计](restore-audit.md)。
