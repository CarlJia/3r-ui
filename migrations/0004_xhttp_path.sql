-- L4: 防止被动扫描发现 REALITY 入站。
--
-- 之前 `xhttpSettings.path` 写死为 `/`，相当于在公网上把入站端口暴露在
-- `GET /` 上：任何人扫到端口都知道这是 xhttp 端点，结合 REALITY fallback
-- 行为即可触发 dest SSRF（见 validate_dest）或主动枚举 shortId。
--
-- 把 path 抽到数据库，存为每个入站独立的随机路径（形如 `/r/<16hex>`），
-- 服务端渲染 config 时用它，分享链接分享时带它，客户端只有拿到链接才
-- 能命中入站。新入站由面板生成；存量入站本次 backfill 一条随机路径。
--
-- 注意：升级后旧分享链接（path 为空，客户端默认请求 `/`）将无法命中入站。
-- 这是关闭泄露点的必然代价——运维需要在升级后重新分发分享链接/二维码。

ALTER TABLE inbounds ADD COLUMN xhttp_path TEXT;

-- 任何没有 path 的入站（迁移前已有的行、或新建时漏掉的）补一条随机路径。
-- randomblob(8) + hex() → 16 hex chars，足够防主动枚举。
UPDATE inbounds
   SET xhttp_path = '/r/' || lower(hex(randomblob(8)))
 WHERE xhttp_path IS NULL OR xhttp_path = '';

