# 手工接入仓库

仅用于排查和定制部署。常规接入使用[仓库 CLI](cli.md)。涉及停服务、IAM 和路由变更，先备份并暂停 Push。

以下以 `content/project-b`、稳定 key `project-b`、图片前缀 `/project-b/` 为例。新增必须同时完成 Git 读取权限、发布白名单、S3 写权限和 Nginx URL 映射，仅创建 Gitea 仓库不会自动发布。

## 创建仓库并授权只读拉取

在 Gitea 创建私有仓库 `content/project-b`，默认分支为 main。使用个人密钥建立初始提交，包含 `public/images/` 中至少一张有效图片。首次接入完成前尚不会自动发布。

在服务器读取 Publisher **公钥**：

```sh
cat /app/open-assets/private/publisher/deploy_key.pub
```

在新仓库的设置 → 部署密钥中添加它，保持只读。不要添加私钥，也不要授予 Publisher 写仓库权限。设置 main 分支保护并禁用 force push。

取得真实仓库 ID。在服务器管理员 shell 执行以下只读查询，它只输出 ID：

```sh
cd /app/open-assets
python3 - <<'PY'
import base64, json, pathlib, urllib.request
p = pathlib.Path('private')
auth = base64.b64encode(('content:' + (p/'admin_password').read_text().strip()).encode()).decode()
r = urllib.request.Request('http://127.0.0.1:13000/api/v1/repos/content/project-b', headers={'Authorization': 'Basic ' + auth})
print(json.load(urllib.request.urlopen(r))['id'])
PY
```

## 更新白名单

安排短暂发布维护窗口，暂停 Push；先停接收器和 Worker，避免配置变更与执行交错：

```sh
docker compose stop webhook publisher
cp -p private/repositories.yaml private/repositories.yaml.before-project-b
```

编辑 `private/repositories.yaml`，保留原项，在 repositories 下追加：

```yaml
  - key: project-b
    repository_id: 123  # 替换为刚查询的真实 ID
    name: content/project-b
    remote: ssh://git@gitea:2222/content/project-b.git
    branch: main
    enabled: true
    serve: true
    source_directory: public
    mount_path: /project-b/
    max_files: 10000
    max_bytes: 1073741824
    max_file_bytes: 104857600
```

不要把 `state_root`、`images_root` 或内部 HTTPS endpoint 改成示例值。ID、key、URL 前缀不可重复；除根路径外，前缀不嵌套。配置文件保持 Publisher 可读：

```sh
chown 20010:20010 private/repositories.yaml
chmod 0640 private/repositories.yaml
docker compose run --rm --no-deps publisher validate
```

## 扩展指定 S3 前缀权限

初始化配方把发布策略保存在 `private/oc-cli/publisher.json`。先备份，再只追加新仓库对象前缀，保留其他语句和 Resource：

```sh
cp -p private/oc-cli/publisher.json private/oc-cli/publisher.json.before-project-b
python3 - <<'PY'
import json, pathlib
p = pathlib.Path('private/oc-cli/publisher.json')
c = json.loads(p.read_text())
s = next(s for s in c['Statement'] if 's3:PutObject' in s['Action'])
resources = s['Resource'] if isinstance(s['Resource'], list) else [s['Resource']]
new = 'arn:aws:s3:::attachments/repositories/project-b/*'
if new not in resources:
    resources.append(new)
s['Resource'] = resources
p.write_text(json.dumps(c, indent=2))
PY
set -a
. ./.env
set +a
docker run --rm --network asset-stack_admin \
  -v "$PWD/private/oc-cli:/config" \
  -v "$PWD/private/tls/ca.crt:/ca.crt:ro" \
  "$OC_IMAGE" --config-dir /config admin policy add store assets-publish /config/publisher.json
```

这是已有专用发布策略的更新，不把 Publisher 改为 root 或 consoleAdmin。不需要重新设置匿名策略：默认公开策略已允许读取 attachments/repositories 下的对象。若你的部署收紧过匿名策略，应另行为新前缀增加只读授权。

## 生成并应用图片映射

先生成候选配置，再让 Nginx 校验。原服务配置保存在私有备份中：

```sh
docker compose run --rm --no-deps \
  -v "$PWD/private/generated:/generated" publisher render /generated/.candidate
cp -p private/generated/images.conf private/images.conf.before-project-b
cp private/generated/.candidate/images.conf private/generated/images.conf
if docker compose exec -T nginx nginx -c /etc/nginx/open-assets.conf -t; then
  docker compose exec -T nginx nginx -c /etc/nginx/open-assets.conf -s reload
else
  cp -p private/images.conf.before-project-b private/generated/images.conf
  echo '配置已恢复；停止操作并检查错误。'
fi
```

校验失败时不要继续。配置会把 `/project-b/` 映射到该仓库 current，未匹配的文件不会回退到根仓库。

重新创建接收器与 Worker，确保单文件 bind mount 在编辑器替换配置 inode 后也读取新配置：

```sh
docker compose up -d --no-build --force-recreate webhook publisher
```

## 配置 Hook 并首次发布

在 Gitea 新仓库设置中创建 Gitea 类型 Webhook：

- 目标：`http://webhook:9000/hooks/publish-assets`，仅内部可达。
- 内容类型：application/json；事件：Push；启用。
- Secret：与服务器 `private/webhook_secret` 文件中的值完全一致，仅在可信管理页面填写。

不要增加公网 Webhook 路由。Gitea 与接收器通过独立 hooks 网络连通，出站白名单限定 webhook 主机及其配置 IP；无需放开全部私网地址。

之后 Push 新提交，或使用新仓库 main 的完整 SHA 手动补发：

```sh
sudo ./scripts/open-assets publish project-b FULL_COMMIT_SHA
sudo ./scripts/open-assets status
```

确认该 SHA 为 published 后访问：

```text
https://img-node-name.lab.dev/project-b/logo.png
https://oss-node-name.lab.dev/attachments/repositories/project-b/releases/<完整SHA>/images/logo.png
```

这里对应新仓库文件 `public/images/logo.png`。确认旧仓库仍可发布，再结束维护窗口；窗口内失败的 Hook 投递需重投或按 SHA 补发。
