# 独立 Rust 全栈验收

入口为 `rust/examples/fresh_stack.rs`，实现和测试均不需要 Python。只允许显式启用的 Linux root 环境；它拒绝已有应用 private、业务数据或任何已有容器的 Docker daemon。

先在可丢弃的独立 Linux 环境将源码放入 `/app/open-assets`，编译：

```sh
cargo build --locked --release --bin open-assets --example fresh_stack
sudo env OPEN_ASSETS_FRESH_TEST=1 sh tests/e2e/fresh-stack.sh
```

不要在生产或个人已有 Docker 环境运行，不要为了让保护检查通过删除既有容器、配置或数据。GitHub `fresh-stack` 使用独立托管 runner 执行相同 Rust 程序。

验收包括真实初始化、SSH 推送与签名通知、增量发布、不可变 inode 复用、危险 SVG 拒绝、图片回退、专用测试对象缺失后的恢复、本地快照重建、清理预览、PNG/SVG 安全响应、HTTPS 与管理认证、Info 和运行镜像中不存在 Python。涉及删除的请求严格绑定临时测试仓库的明确对象，不增加生产任意删除命令或扩大 Publisher 权限。

密钥、密码和诊断只留在一次性环境的 `private/`，不能上传为 CI artifact。失败记录必须区分未完成的阶段；编译与单元测试通过不能代替此验收。

## 新入口与网页分享配置

此锚点保留供历史验收记录引用。旧版脚本保存在对应历史提交；当前使用上面的 Rust 验收入口。网页分享的公开前缀需按[网页上传配置](../../docs/web-uploads.md)显式配置，不能自动公开整个存储桶。
