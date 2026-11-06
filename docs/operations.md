# 运行维护

```sh
docker compose exec publisher python -m open_assets status
docker compose exec publisher python -m open_assets publish main-images COMMIT_SHA
docker compose exec publisher python -m open_assets retry JOB_ID
docker compose exec publisher python -m open_assets rollback-images main-images COMMIT_SHA
```

publish 创建显式人工任务；自动任务只有目标等于当前 main 才生效。retry 只处理 failed 任务。Worker 重启会恢复 running/images_prepared。全局文件锁保证单写入，首次版本只部署一个 Worker。任务失败仅记录错误类型以避免秘密泄漏，诊断需在私有环境执行。

current 通过同文件系统内软链接替换。回滚校验保存清单和本地字节，不删除附件。回滚不会阻止后续合法 Push 更新；维护期间先停止自动发布。不要手工修改版本目录。

备份数据、IAM/config、Gitea 数据库和仓库、发布清单及任务、私有凭据与 CA、Traefik ACME。数据库与对象状态需一致性快照，备份存到独立设备并演练恢复。不得将生产归档纳入开源仓库。

无自动历史清理。监控磁盘余量、失败任务、队列、证书有效期和备份年龄。部署前确保容量足够保存 staging、对象、图片和 Git 历史。

`scripts/snapshot.sh` 提供单机一致性快照：暂停应用写入组件，归档应用、数据和指定 Traefik/ACME 目录，校验后恢复。需要计划维护窗口。Traefik 本身继续提供服务；ACME 状态若有并发写入，应单独安排其一致性备份。快照仍在同机时不能防止整机或磁盘损坏，必须另行配置加密异机复制。
