# 网页上传并公开分享

默认 OC 可以管理有权限的存储对象，但上传不会同步到 Git，也不会更新 Nginx 图片目录。本页由服务器管理员一次性创建独立的 `uploads` 桶，仅允许匿名读取 `public/` 前缀。之后使用者可在 OC 上传到这个目录，直接获得公开链接。

此配置会让 `uploads/public/` 中的文件无需登录即可读取。只上传准备公开的内容，知道地址的人即可下载；关闭目录列表不能让已知文件地址保密。公开文件可能包含 HTML 等主动内容，只给可信账号上传权限；OSS 与管理界面保持不同主机名，不扩大管理 Cookie 的作用域。Git 的 `attachments/repositories/`、清单和恢复记录保持独立，不在其中手工上传。

## 1. 检查前提

先完成[部署](deployment.md)或[新服务器路线](new-server.md)，确认 OC 登录和原有 OSS 入口正常。下面在 `/app/open-assets` 的管理员 shell 执行，使用初始化生成的私有 OC 管理 alias；不用在命令行填写 root 密码。

```sh
sudo -i
cd /app/open-assets
set -a
. ./.env
set +a
oc_admin() {
  docker run --rm --network asset-stack_admin \
    -v "$PWD/private/oc-cli:/config" \
    -v "$PWD/private/tls/ca.crt:/ca.crt:ro" \
    "$OC_IMAGE" --config-dir /config "$@"
}
```

`.env` 是自己的受保护运行配置，只加载可信文件。alias 使用内部 HTTPS 和私有 CA；不要使用 insecure 选项。已有部署先按[维护窗口](operations.md#维护窗口)协调并行管理操作，并备份应用配置、存储 IAM/桶策略和 Traefik 动态文件。

本例只适用于尚无 `uploads` 桶的部署。如果桶或下述文件已存在，先检查用途与策略，不覆盖已有资源，也不为重做教程删除桶。

## 2. 创建专用桶与只读公开策略

```sh
oc_admin mb store/uploads || { echo '创建未成功；检查是否已有同名桶，停止后续步骤。' >&2; exit 1; }
test ! -e private/oc-cli/uploads-public.json || { echo '策略文件已存在，先检查。' >&2; exit 1; }
cat > private/oc-cli/uploads-public.json <<'EOF_POLICY'
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Principal": "*",
    "Action": ["s3:GetObject"],
    "Resource": ["arn:aws:s3:::uploads/public/*"]
  }]
}
EOF_POLICY
chmod 0600 private/oc-cli/uploads-public.json
oc_admin policy set-json /config/uploads-public.json store/uploads
```

匿名权限只有 `GetObject`，没有列桶、上传或删除权限。该命令设置的是新桶 `uploads` 的策略，不替换 Git 使用的 `attachments` 桶策略。如果设置失败，先修复原因，不继续公开路由。

初始 OC 身份绑定 `consoleAdmin`，可用于管理员验证。多人日常使用应建立各自 IAM 用户并限定上传权限；不要把存储 root 或 Publisher 凭据交给使用者。本页不自动创建多人账号或多租户隔离。

## 3. 为这个前缀增加公开路由

设置现有 Traefik file provider 实际监视的宿主目录。新服务器路线使用 `/app/open-assets-traefik/dynamic`；复用入口时替换为自己的目录。原有 `open-assets.yaml` 必须已由 `configure-edge.py` 生成，其中提供内部 TLS 校验的 `objects` 服务。

```sh
export TRAEFIK_DYNAMIC_DIR=/app/open-assets-traefik/dynamic
python3 - <<'PY'
import json, os
from pathlib import Path
root = Path('/app/open-assets')
directory = Path(os.environ['TRAEFIK_DYNAMIC_DIR'])
if not directory.is_dir():
    raise SystemExit('动态目录不存在，停止操作')
target = directory / 'open-assets-uploads.yaml'
domains = json.loads((root / 'private/domains.json').read_text())
route = {'http': {'routers': {'uploads-public': {
    'rule': f"Host(`{domains['oss']}`) && PathPrefix(`/uploads/public/`) && (Method(`GET`) || Method(`HEAD`))",
    'entryPoints': ['websecure'],
    'service': 'objects@file',
    'tls': {'certResolver': 'le'},
}}}}
# Exclusive creation avoids replacing an existing routing file.
with target.open('x') as stream:
    json.dump(route, stream, indent=2)
# Routing contains no credentials; let the non-root gateway read it.
target.chmod(0o644)
PY
```

这是 JSON 语法的 YAML，file provider 可加载；后缀保持 `.yaml`。没有去掉路径前缀，也没有开放 S3 根路径、管理 API 或写入方法。它不经过 Stargate；后端仍使用已有 `storage-tls` CA 校验。生成器不检查其他动态文件是否已使用 `uploads-public` 名称，有冲突先协调命名。

Traefik 应自动重载目录。查看其所属 Compose 项目的日志，确认没有路由、服务引用或 TLS 错误；不要重新生成原文件或重启所有容器来掩盖错误。

## 4. 上传并获取链接

在自己的电脑准备一份不含秘密的 `hello.txt`，例如内容为 `Hello Open Assets`。登录 OC，打开 `uploads`，进入或创建 `public` 目录，再上传文件。确认对象 key 是 `public/hello.txt`，不是文件名里包含重复的 public 路径。

在自己的电脑执行，将主机名替换为实际 OSS 地址：

```sh
export OSS_HOST=oss-node-name.lab.dev
export SHARED_URL="https://$OSS_HOST/uploads/public/hello.txt"
curl --fail --head "$SHARED_URL"
curl --fail "$SHARED_URL" -o hello.downloaded.txt
cmp hello.txt hello.downloaded.txt
```

HEAD 与 GET 应成功，cmp 成功时没有输出。使用无痕窗口打开相同地址，无需 Stargate/OC 登录即可下载。中文、空格、`+`、`%` 需按路径编码，方法见[使用指南](usage.md#5-获取图片与附件链接)。Git 的 `links` 命令不用于这个独立上传桶。

再确认边界：

```sh
# 列桶地址未配置公开路由，不应返回 2xx 或对象清单。
curl --silent --output /dev/null --write-out '%{http_code}\n' "https://$OSS_HOST/uploads?list-type=2"
# 仅以 HEAD 检查未公开前缀，不创建测试对象；不应返回 2xx。
curl --silent --head --output /dev/null --write-out '%{http_code}\n' "https://$OSS_HOST/uploads/private/hello.txt"
```

通常返回 404；已有入口的其他路由也可能返回 403。若出现 2xx，先调查额外路由或策略，不扩大匿名授权继续排查。只测这两个地址不能证明全部 IAM 边界，匿名写入限制还应通过审阅策略确认，不在业务文件上尝试覆盖或删除。

## 5. 更新、撤下与备份

网页上传没有 Git 版本清单和发布任务。同名上传可能覆盖原对象，不享有 Git 恢复 CLI 的重建保证；需要长期版本链接时使用[Git 发布](first-share.md)。响应缓存与客户端可能让旧内容暂时保留。

撤下单个文件由有权限的管理员在 OC 处理，并先确认备份及外部引用。停止整个公开目录时，将本次新增动态文件移出 file provider 目录，确认新请求不再命中；保留原文件作回退，不删除 `acme.json`。路由撤下不会清除客户端已有缓存，也不会自动撤销桶策略或通过其他入口获得的访问。

若需要同时撤销本例的桶匿名策略，先保存并核对桶当前策略，仅移除本例的 `GetObject` 授权，保留后来添加的其他语句，再通过 `policy set-json` 应用。不要把创建空策略或清空桶作为通用撤回步骤。

本桶不进入仓库级容量统计、Git 版本保留或恢复 CLI；主机磁盘仍会反映它的占用。将对象和桶策略纳入独立备份、容量监控及生命周期计划，避免以为仓库清理任务也会管理这些网页上传内容。
