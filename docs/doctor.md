# 部署诊断

在部署服务器执行：

```sh
cd /app/open-assets
sudo ./scripts/open-assets doctor
sudo ./scripts/open-assets doctor --json
```

命令检查配置、八个应用容器、定时器及其 service、Info 快照和四类统计缓存。它只读配置、固定缓存文件与 systemd/Docker 状态，不启动服务、不遍历版本目录、不访问 S3，也不修改凭据或删除数据。

正常返回 0；存在待处理项返回 1。可选定时器未启用本身不影响退出码；读取状态失败、已记录的执行失败仍需处理。JSON 的 `checks` 是分类结论，`timers` 和 `caches` 提供逐项结果、原因与操作提示。`components` 保留容器状态。输出和日志可能包含部署路径、仓库 key，分享前仍需脱敏。

## 必需与可选定时器

必需项是完整部署的采集与日志维护要求，不表示停止它们会立即阻止图片访问：

- `info`：主机快照，也负责更新本地版本统计缓存。
- `objects`：对象数量与占用采集。
- `audits`：恢复审计数量与占用采集。
- `resources`：Git 缓存与全局队列采集。
- `logrotate`：私有部署日志轮转；需按[日志配置](logging.md)安装配置和单元。

以下功能按需启用：

- `repair`：自动修复异常容器，见[健康检查](health.md)。
- `retention`：本地历史版本清理，见[本地保留](retention.md)。
- `object-retention`：对象保留预览，见[对象保留](object-retention.md)。单元本身不传入删除开关。

可选项关闭时显示 `not-enabled`，JSON `status` 为 `optional`。不要为了让检查通过而启用清理或自动修复。必需项未启动时会提示安装并启用对应 timer；启用前仍应完成文档中的配置。

## 调度与执行分别检查

`active` 只表示 timer 正在调度。doctor 还读取对应 service 的结果、退出码、启动与结束时间，以及条件检查结果：

- `ok`：最近执行成功，且完成时间未超限。
- `running`：服务正在执行，完成后需再检查。
- `failed`：执行失败、超时、信号终止或非零退出码。先查看该 service 的日志。
- `condition-skipped`：本次启动被条件拒绝。例如维护标记存在时，部分单元不会执行；旧的成功结果不抵消这次跳过。
- `never-or-unknown`：本次开机以来没有可确认的完整执行，或时间记录异常。
- `overdue`：最近成功完成时间过久。
- `unavailable`：单元状态缺失、不可读取或格式异常。确认单元安装、systemd 可用及读取权限。

完成时间使用本次开机的单调时钟计算，不能追溯重启前已丢失的 systemd 执行状态。最大完成年龄为：info 120 秒，objects/audits 2100 秒，resources/repair 180 秒，logrotate 7800 秒，两个保留任务 46800 秒。这些阈值包含当前单元调度间隔和余量；自定义 systemd 周期时需同时调整诊断阈值。

新启用但尚未首次运行也会提示，不会默认当作成功。`private/maintenance.flag` 存在时单独提示维护状态；不要只为消除提示删除标记。

## 缓存检查

缓存年龄按各自 `measured_at` 计算，不采用文件修改时间，也不采用外层快照的采集时间：

- `info-snapshot`：主机快照，超过 120 秒提示。
- `local-versions`：`repositories-cache.json`，逐仓库检查，超过 600 秒提示；配置摘要变更或上轮测量失败也提示。
- `objects`：`objects-cache.json`，逐仓库检查完整采样，超过 1800 秒提示。正在分页采集显示 `collector-scanning`，不会把不完整数据当作成功。
- `audits`：`audit-cache.json`，逐仓库检查，超过 1800 秒提示。
- `resources`：`resource-cache.json`，检查全局样本，超过 `INFO_RESOURCE_INTERVAL_SECONDS` 的两倍提示，默认 1800 秒。预算或仓库范围变更会使旧缓存失效。

缺失、损坏、仓库采样不全、采集失败或锁忙均会提示。`clock-skew` 表示采样时间比当前时间超前超过 5 秒，应先检查系统时间。新鲜的旧样本不能掩盖最近采集失败；Info 快照新鲜也不能抵消底层缓存过期。

缓存阈值表示统计是否足够新，不证明文件或对象完整，也不检查业务内容、外部 DNS、外部 Traefik、证书链或备份可恢复性。清理和恢复操作报告不是周期统计缓存，仍需按各自 CLI 核对。

## 按提示处理

先查看对应日志，例如：

```sh
sudo systemctl status open-assets-objects.timer open-assets-objects.service --no-pager
sudo journalctl -u open-assets-objects.service -n 50 --no-pager
```

检查维护状态、权限、磁盘空间、网络、凭据或采集预算，解决原因后可以显式补跑采集，再检查：

```sh
sudo systemctl start open-assets-objects.service
sudo ./scripts/open-assets doctor
```

doctor 只为采集和日志轮转提供这种补跑提示。对自动修复及两个保留任务，先核对策略和实际影响，不直接重试。锁忙或对象分页未完成时可先等待下一轮；持续提示再查看日志与预算。不要重跑初始化、放宽权限或关闭删除保护来修复统计问题。

## 在 Info 中查看

Info 的“部署诊断”页签通过独立定时任务缓存本命令结果，默认五分钟采样一次。页面刷新不触发检查，最近执行结果与缓存年龄均标明为采样时记录。采集失败或结果过期时保留旧报告并提示，不显示为当前通过；安装和配置见[Info 部署诊断](info.md#部署诊断)。独立采集器本身不加入这八个任务的报告，其状态和诊断缓存的新鲜度另行展示。
