# ENOTCONN / 挂载 ready 语义根因记录（2026-03-09）

## 结论先行

`Transport endpoint is not connected (os error 107)` 在这条链路里不是单一问题，而是 **Scorpiofs/FUSE 挂载 ready 语义不够强** 与 **Buck2 对工作目录可达性要求更高** 叠加后的表现。

当前真实链路已经能证明：

- Antares `mount_job()` 返回时，FUSE session 已经启动，但这不等价于 Buck2 立刻可以安全执行 `audit cell / audit config / targets / build`。
- 如果 Orion 只验证“目录存在 / current_dir 可进入”，仍可能在后续 Buck2 target discovery 阶段撞到：
  - `.buckconfig` 尚未真正可见或可读；
  - 深层目录遍历仍未完全 ready；
  - Dicfuse/import 后台加载仍在推进，Buck2 访问会出现短时 ENOENT / ENOTCONN / 卡住。

## 这和 `os error 107` 的关系

`os error 107` 说明用户态进程访问的 FUSE mount 端点已经不再可靠，典型表现是：

- mount 已创建，但底层 session 已断开；
- mount 已返回，但真正访问某些路径时连接失败；
- 目录浅层可访问，但 Buck2 一上来做更深的路径解析和 cell 发现时失败。

因此，`ENOTCONN` 不是 “daemon 概念错了”，而是 “Buck2 在一个还不够 ready 或已经失活的 FUSE 视图上工作”。

## 上游代码层面的关键信号

基于本地 `scorpiofs` 源码检查，可以归纳出三点：

1. `mount()` 返回时代表 FUSE session 已经被 spawn，但 Dicfuse 后台导入仍可能继续。
2. 现有 ready 语义更接近“根 inode / 根目录已挂上”，而不是“Buck2 所需路径树已稳定可读”。
3. Buck2 这类负载不是只看 mountpoint 是否存在，而是会立即做：
   - `.buckconfig` / `.buckroot` 发现；
   - cell resolver 初始化；
   - 浅层到深层目录遍历；
   - 后续 daemon/materializer 读写状态目录。

## 本次 Orion 侧已落地的缓解

本次真实联调中，Orion 侧已经把 mounted repo ready probe 从“只验证目录存在”加强为“至少要能访问并打开 `.buckconfig`”。

这带来了两个直接效果：

- 不会再把一个“目录刚出现但 Buck2 project root 还没 ready”的 mount 误判为 ready；
- 之前偶发的 `Couldn't find a buck project root` 被前移成更明确的 ready 等待，语义更准确。

但这仍然是 **Orion 侧补强**，不是 Scorpiofs 上游 ready 合约本身已经足够强。

## 仍建议 Scorpiofs 上游演进的方向

### 1. 提供更强的 ready 合约

建议把 ready 从“mount 已建立”提升为“工作树关键 Buck 根文件已经可达”，至少覆盖：

- mountpoint 根目录可读；
- `.buckconfig` 可 stat / open；
- `.buckroot` 可 stat；
- 至少一轮浅层目录树导入完成。

### 2. 对外暴露 path-scoped readiness 能力

相比让上层自己轮询路径，Scorpiofs 更适合直接提供：

- `wait_until_ready(job_id, path)`；或
- `mount_job(...).await` 直到 path 达到 ready 条件才返回；或
- 一个显式 health/readiness API，返回当前 mount 的加载阶段。

### 3. 区分“session 活着”和“路径 ready”

上层真正关心的是两个状态：

- FUSE session 是否仍存活；
- 指定 repo path 是否已经 ready 给 Buck2 使用。

这两个状态需要被显式区分，否则上层只能把所有失败都归结为同一个挂载问题。

### 4. 排查 `rfuse3` 4096 对齐 warning

真实链路里仍能看到：

- `rfuse3::raw::session: The data is not 4096 bytes aligned`

目前还没有证据表明它直接导致功能错误，但它说明底层 I/O 路径还有实现细节值得继续核查，建议作为 Scorpiofs/FUSE 层的独立稳定性事项追踪。

## 当前最合理的分层结论

- **L1：挂载 ready / session 可达性问题**
  - 典型症状：`ENOTCONN`、`Couldn't find a buck project root`、访问直接卡住
- **L2：Buck2 高写入状态目录不适合放在 FUSE upper layer**
  - 典型症状：daemon lock / SQLite / advisory lock I/O 问题
- **L3：Scorpiofs 自身 store/state 生命周期问题**
  - 典型症状：`path.db` WouldBlock、残留 mount/state 不一致

`ENOTCONN` 主要属于 **L1**，而不是 L2/L3。

## 当前建议

短期内：

- 上层继续保留更强的 ready probe；
- 继续把 Buck2 `buck-out` 放在宿主机本地稳定目录；
- 对 target discovery 失败保留 fresh mount retry。

中期内：

- Scorpiofs 上游补强 ready 合约；
- 把 “session alive” / “path ready” / “import progress” 做成显式可观察状态；
- 单独追踪 `rfuse3` 对齐 warning 和深层路径访问偶发卡住问题。

## 2026-03-09 晚间补充：本轮结构化修复与建议

### 1. ready contract 进一步上收到了 Scorpiofs mount 语义里

本轮没有继续让 Orion 在 mount 返回后自己猜 repo 是否 ready，而是在 Scorpiofs 的 `AntaresManager` 中新增了：

- `mount_job_for_path_with_ready_path(...)`
- `mount_job_at_for_path_with_ready_path(...)`

做法是：

- 在 Dicfuse 侧，先对指定 `ready_path` 调 `wait_for_path_ready()`；
- 在 FUSE mount 成功后，再对实际 mountpoint 上的同一路径做一次 post-mount probe；
- Orion 挂 Buck repo 时显式传入 `/.buckconfig`。

这意味着：

- `ready` 不再只是“根挂上了”；
- 而是“对 Buck 来说关键的 repo-root sentinel 已经在挂载视图里可见且可访问”。

这正是之前 `Timed out waiting for mounted repo path ... old-1 ... No such file or directory` 这类问题应该落的层。

### 2. 为什么这次是小重构，而不是继续打补丁

因为这次处理的是 **边界语义**，不是继续加 if/else：

- 之前 Buck root ready 的判断散在 Orion；
- 现在把这个约束收回到 Scorpiofs mount contract；
- Orion 自己只保留 mount liveness / retry，职责更单一。

这样后续如果还有别的上层也要拿 Scorpiofs 给 Buck-like workload 挂 repo，它们不需要重复实现一套 `.buckconfig` 等待逻辑。

### 3. `rfuse3::raw::session: The data is not 4096 bytes aligned`

本轮进一步确认：

- 这条 warning 的直接打印点在 `rfuse3` 上游；
- 当前更像是写 buffer alignment 的噪音告警，而不是 Scorpiofs 业务逻辑错误的直接证据；
- 因此 Scorpiofs 这一层没有继续为了“消一条日志”去改本身架构。

更合理的处理方式是：

- 在业务侧日志里临时降噪，避免误导排障；
- 同时把根因明确记录为 `rfuse3` 上游事项；
- 后续若要彻底消掉，应提交上游 patch，而不是继续在 Scorpiofs/Orion 里堆绕路逻辑。

### 4. 当前对 ENOTCONN 这一层的新结论

现在可以把 ENOTCONN/ready 问题再压缩成一句话：

- **不是 Buck2 不该跑在挂载 repo 视图上**；
- 而是 Scorpiofs 需要更强的 ready contract，确保“挂给 Buck 的 repo 视图”在返回时已经满足 Buck root 发现的最低条件。

这次修完后，真实 worker 回归里：

- 没再复现 `old-repo mount ready timeout`；
- 没再看到之前那类 `No such file or directory` 的 mount-ready 失败；
- 但 `buck audit cell` 在真实 FUSE 视图上仍可能比较慢，这更像后续性能/延迟项，而不是 ready correctness 缺陷。

### 5. 对 Scorpiofs 上游 patch 方案的建议清单

如果后续要对外整理成更明确的上游 patch 方案，建议拆成三块：

1. **ready contract**
   - 保留 `wait_for_path_ready()`
   - 对 `AntaresManager` 暴露 path-scoped readiness API
   - mount 返回前完成 path-scoped post-mount probe

2. **恢复态一致性**
   - DB recovery 时不要把目录恢复成 fresh/loaded 的错误状态
   - 恢复后目录应重新走可遍历性确认，而不是直接当成 ready

3. **rfuse3 噪音告警**
   - 单独向上游提交 `handle_write` 对齐告警的修正
   - 不建议把这个问题混进 Scorpiofs 业务逻辑修复里
