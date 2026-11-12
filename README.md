# blobdev

把对象存储（S3、WebDAV、本地目录）上的大文件，当作**块设备**挂载使用。

```text
文件系统 / 应用 ──► /dev/ublkbN ──► blobdev（用户态，ublk）
                                      ├─► 本地缓存（可丢，加速 + 攒批）
                                      └─► 对象存储（权威数据，按 chunk 分片）
```

`ublk` 之于块设备，相当于 `FUSE` 之于文件系统：内核把块 IO 转发给用户态程序，
程序只需要实现“读这一块、写这一块”。

## 为什么

对象存储便宜、容量大、耐用，但只能整对象读写，还有网络延迟。
把它的数据当成一块“盘”用，是把冷数据、镜像、备份接进现有系统的省事做法。
本地缓存吸收延迟，攒批上传对付写放大。

## 架构

参考 JuiceFS 的思路，分成互相独立的两层：

| 层 | 职责 | 本项目的实现 |
|---|---|---|
| **元数据** (`src/meta.rs`) | 卷几何、哪些 chunk 被改过、脏块位图 | 内存 / 本地文件（原子替换 + CRC） |
| **对象存储** (`src/store.rs`) | 只存取 chunk 数据对象 | 目录 / S3 / WebDAV |

元数据与数据分开的好处很直接：一次写只更新本地元数据，**永远不等网络**；
元数据后端可以按需要换成数据库或分布式 KV；对象存储只管搬数据。

`src/volume.rs` 组合两者，实现读写、缓存、脏块追踪、上传策略。
`src/ublk_tgt.rs` 把卷暴露成块设备。

### 对象布局

```text
<bucket>/<prefix>/<volume>/c/u<uid>    整体替换写入的对象（默认 4 MiB，最后一个可能不满）
<bucket>/<prefix>/<volume>/c/<index>   原地修改的对象（WebDAV / 本地目录），按对象索引命名
```

两种命名靠 `u` 前缀分开，因为 **uid 和对象索引会落在同一数值范围**：不加前缀时
`c/4` 既是“uid 4 新写的对象”，又是“object 4 的原地修改版本”，两个对象会互相覆盖
和删除。旧版本写的卷用 `c/<uid>`，打开时自动识别并沿用那个拼法。

### 写策略取决于后端能力

`ObjectStore::caps()` 声明后端能做什么，卷据此选策略：

| 后端 | `partial_write` | 上传方式 | 4 KiB 写的实际流量 |
|---|---|---|---|
| S3 | 否 | 整个 object 替换（4 MiB） | 4 MiB（放大 1024×） |
| WebDAV | 是 | 只重写脏块区间 | 4 KiB（约 1×） |
| 本地目录 | 是 | 只重写脏块区间 | 4 KiB |

这是选择后端时最该看的一条：**S3 的对象不可变，改一个块就要重传整个 chunk；
WebDAV 支持 `Content-Range`，可以只写改动的部分。**

### 读缓存

读未命中时，按请求大小分两种策略：

| 情况 | 行为 |
|---|---|
| 读覆盖 object 的 ≥ `--whole-chunk-pct`（默认 25%） | 拉整个 object 并缓存（顺序读最优） |
| 更小的读 | **只拉需要的字节区间**（range read） |

没有 range read 的话，S3 上读一个 4 KiB 块要传 4 MiB，放大 1024 倍。

**两级缓存**：

- **内存 LRU**（`--read-chunks`，默认 64 个 object）：热数据，零 IO
- **磁盘缓存**（`--cache-dir` + `--cache-size`）：**重启后仍然命中**，容量不受内存限制

磁盘缓存每个 chunk 一个文件，带 CRC 和长度校验；**校验失败只当未命中并重删**，
绝不会把损坏数据返回给上层。写入走临时文件 + rename，崩溃不会留下半截文件。
超出预算时按 LRU 淘汰。

```bash
# 20 GiB 磁盘读缓存
blobdev run ... --cache-dir /var/cache/blobdev --cache-size 20G
```

### 上传是写时替换（copy-on-write）

**S3 类后端**（对象不能原地修改）：每个 chunk 的数据对象都带一个**全新的 uid**，
从不覆盖已有对象。

```text
1. 写入新对象  <c>/<新uid>       ← 旧的 <旧uid> 完全不动
2. 落盘元数据  chunk_id -> 新uid  ← 唯一真正的切换点
3. 删除旧对象  <旧uid>           ← 失败也无害，只是浪费空间
```

任何一步中断都不会损坏数据：1 号步骤失败时旧对象还在，2 号步骤失败时旧对象也还在。
**只有元数据落盘成功，旧对象才变得可回收**。

**WebDAV / 本地目录**（支持 `Content-Range`）：直接 patch 脏区间，一个请求原子完成。

### 什么时候上传

| 触发 | 默认 | 说明 |
|---|---|---|
| chunk 脏块占比达阈值 | 50% | 攒够批量再传，减少请求数 |
| 脏了太长时间 | 30 秒 | 少量随机写也能及时落盘 |
| FLUSH / FUA | — | 文件系统 `fsync` 时强制上传全部 |
| 进程退出 / 收到信号 | — | 退出前完整上传 |

## 用法

```bash
# 本地目录后端（不需要外部服务，适合试跑）
blobdev run --backend dir --dir /var/lib/blobdev --volume vol1 \
            --create-size 10G --meta /var/lib/blobdev/vol1.meta

# S3 / S3 兼容（RustFS、MinIO、Ceph RGW …）
blobdev run --backend s3 --endpoint http://127.0.0.1:9000 --bucket blobs \
            --volume vol1 --access-key KEY --secret-key SECRET --path-style \
            --create-size 10G --meta /var/lib/blobdev/vol1.meta

# WebDAV
blobdev run --backend webdav --endpoint https://dav.example.com/files \
            --user U --password P --volume vol1 --create-size 10G \
            --meta /var/lib/blobdev/vol1.meta

blobdev stop   --id 0     # 卸载设备
blobdev status            # 列出 ublk 设备
blobdev inspect --volume vol1 ...   # 校验元数据与实际对象是否一致
blobdev ls      --volume vol1 ...   # 列出 chunk 对象
blobdev gc      --volume vol1 ...   # 回收无引用的孤儿对象（先加 --dry-run 看）
```

`inspect` 会逐个检查元数据引用的对象是否真的存在；`gc` 只删除没有任何引用的对象，
并且在发现引用缺失时会拒绝执行（那时卷已经不一致，删掉剩余对象只会让情况更糟）。

**一个卷只允许一个进程**：`run` 会在元数据旁加 `.lock` 文件独占锁，
第二个实例会直接报错退出，而不是两个进程互相覆盖元数据。

`--create-size` 只在卷不存在时生效，之后从元数据读几何信息。

挂上之后就是普通块设备：

```bash
mkfs.ext4 /dev/ublkb0
mount /dev/ublkb0 /mnt
cp bigfile /mnt/
umount /mnt          # 触发 flush，所有脏 chunk 上传
```

## 运维

systemd 单元与安装说明见 [`packaging/INSTALL.md`](packaging/INSTALL.md)：

```bash
install -m 0755 target/release/blobdev /usr/local/bin/blobdev
install -m 0644 packaging/blobdev@.service /etc/systemd/system/
cp packaging/blobdev.conf.example /etc/blobdev/data.conf   # 填后端与密钥
systemctl enable --now blobdev@data
```

`SIGTERM` 会先把所有脏 chunk 上传完再退出，`TimeoutStopSec=300` 给足时间。

## 一致性

- **对象存储是权威数据。** 本地缓存丢了只影响速度。
- 元数据在每次写后原子落盘（临时文件 + rename + CRC）。进程崩溃后重启，
  会从元数据恢复“哪些块还没上传”，下次 flush 补齐。
- S3 的 `PutObject` 是原子的，chunk 不会出现“写了一半”的中间状态。
- 元数据文件丢失 = 需要重建卷（chunk 对象还在，但脏块位图没了）。

### 后端故障时应用会看到什么

对象存储不可用时，块设备对每次 I/O **返回 `EIO`**，daemon 日志里能看到具体请求
和原因。不会挂住，也不会返回成功。实测：故障期间对一个已存在的卷发 4 KiB 读，
**0 秒返回 EIO**，daemon 记录 16 条错误（每条带 URL 和状态码），进程本身存活。

**但此时挂不上已有的卷。** 挂载要先读超级块，那是冷数据，必须从对象存储取。
所以对象存储整体不可用时：

- 已经挂载的卷**继续工作**——它按需读，读到没缓存的地方才会报错
- **没挂载的卷挂不上**——包括机器重启之后

daemon 会在故障期间启动（元数据在本地，能读），但 `mount` 会失败。这不是 bug，
是"对象存储是权威数据"的直接结果：没有后端就没有数据。

如果要求"后端长时间不可用期间仍能挂载"，那需要把整个卷常驻在本地——
那是另一个产品（本地副本 + 异步同步），不是当前设计。

但要注意一个内核行为：**`sync(2)` 不会把这个错误传回调用者**。`sync` 把所有
缓存中的数据写下去就返回了，块层记录的写错误只标记在页上，由下一次访问或
`fsync` 报出来。所以写完立刻 `sync` 可能返回 0，而后来的 `fsync` 返回 `EIO`。
这是块设备层的通用行为，本地磁盘拔掉时也一样，不是 blobdev 特有的。

依赖持久化确认的程序应该用 `fsync()` / `fdatasync()`，而不是 `sync()`。

### 后端恢复之后

ext4（和大多数文件系统）在遇到 I/O 错误后会把文件系统**重挂为只读**，即使
后端已经恢复也不会自己变回可写。这是内核的保护策略——它无法判断底层的错误
是暂时的还是永久的。

恢复步骤：

```bash
umount /mnt/data
blobdev inspect --volume data ...   # 可选：确认元数据和对象一致
mount /dev/ublkbN /mnt/data
```

卷本身没有损坏：脏块一直留在 daemon 的内存工作副本和元数据里，后端一恢复
就会补传。实测宕机 8 MiB 未上传数据、恢复后卸载重挂，数据完好。

## 测试

```bash
cargo test                       # 70 单元 + 12 集成测试，不需要网络和内核支持
scripts/e2e-dir.sh               # 端到端（目录后端）
scripts/e2e-s3.sh                # 端到端（S3，需先准备服务端）
scripts/e2e-webdav.sh            # 端到端（WebDAV）
scripts/stress.sh                # 长跑：持续写 + 每轮校验 + 重启复核
scripts/crash-test.sh            # 断电：写入途中随机 kill -9，验证恢复
scripts/restart-cycles.sh        # 反复重启同一卷，验证累积数据不丢
scripts/long-run.sh 600          # 长跑：混合负载 + 采样内存/fd，看是否有泄漏
scripts/fault-injection.sh       # 后端宕机：设备仍响应、fsync 报错、恢复后可用
scripts/e2e-s3-vhost.sh          # virtual-host 寻址端到端
scripts/webdav_server.py DIR     # 本地 WebDAV 测试服务端（纯标准库）
```

端到端脚本都需要 root 和 ublk，会验证：设备容量、mkfs/mount、写入、
缓存读回一致、flush 后对象存储出现 chunk 对象、
**完全重启后从对象存储重读并比对 sha256**。

### 实测结果

在 Linux 7.0.14（`ublk_drv`）+ RustFS（S3，podman）上实际跑过：

| 场景 | 结果 |
|---|---|
| 目录后端：写 120 MiB → flush → 重启重读 | 通过，22 个 chunk 对象，sha256 一致 |
| S3：改一个块 → 旧对象消失、新 uid 出现、总数不变 | 通过，0 错误 |
| S3：写 48 MiB 后重启（drop 页缓存） | 通过，13 个 chunk 直接来自本地缓存 |
| S3：`gc` 识别并删除孤儿，不误删引用中的对象 | 通过（5 引用保留、2 孤儿删除） |
| 两个进程抢同一卷 | 通过，第二个被拒绝并提示持有者 PID |
| **长跑**：S3 连续 120 秒写入并校验 | **992 轮**，0 错误，重启复核通过 |
| **长跑**：WebDAV 连续 90 秒写入并校验 | **721 轮**，0 错误，重启复核通过 |
| **断电**：写入中随机 `kill -9` × 4 轮 | 每轮重启后卷可用，历史数据完好 |
| **写放大**：100 MiB 写入后 drop 页缓存 + 重启复核 | 通过，对象为 4 MiB |
| **反复重启**：同一卷 4 轮，每轮加数据 | 通过，历史文件逐轮完好 |
| **长跑 S3**：10 分钟混合负载 | 381 轮 0 错误，RSS 持平在 166 MB，fd 恒定 |
| **长跑目录后端**：3 分钟 | 225 轮 0 错误，内存与 fd 稳定 |
| **后端宕机**：S3 中途消失再恢复 | 设备不挂住、fsync 报 EIO、重挂后可用 |
| **virtual-host 寻址**：64 MiB / 27 对象 | 通过，重启后逐字节一致 |
| WebDAV：写 40 MiB → flush → 重启重读 | 通过，sha256 一致 |
| S3（RustFS）：写 80 MiB → flush → 重启重读 | 通过，sha256 一致，0 错误 |
| S3：写 512 MiB（4 个文件）→ 重启重读 | 通过，写入耗时 2 秒（不等待上传） |
| S3：写 30 MiB → `sync` → `kill -9` → 重启 | 通过，日志显示 `restoring N chunk(s)`，sha256 一致 |

**崩溃恢复的语义**要分清：`sync` 之后数据已经到达 blobdev（在内核页缓存之外），
此时 `kill -9` 不会丢数据，因为元数据记录了哪些块待上传。
如果在 `sync` 之前就 `kill -9`，数据还在内核页缓存里，那属于文件系统层面的丢失，
与被缓存的磁盘一样，不是 blobdev 能负责的范围。

## 已知限制

1. **元数据只有内存和本地文件**。多客户端共享一个卷需要换成数据库/Redis
   后端（`MetaStore` 是 trait，接口已经留好）。
2. **S3 写放大 1024×**：改一个 4 KiB 块要传 4 MiB。减小 object 能减轻，
   但会增加请求数；没有银弹。
3. **一个卷只能有一个进程**：`run` 用 `.lock` 独占锁，第二个实例直接报错退出；
   但缓存目录本身不跨卷共享，多机部署各自缓存。
4. **同步 IO**：读/写在队列线程里同步完成。慢后端会占用队列线程（但不占锁），
   高并发场景下吞吐受队列数限制（`--queues`）。
5. **元数据随卷大小线性增长，且每次写入都整份落盘**。实测（`cargo test
   metadata_size_for -- --nocapture`）：

   | 卷大小 | 对象数 | 元数据大小（有脏块） |
   |---|---|---|
   | 64 GiB | 16 K | 464 KiB |
   | 256 GiB | 65 K | 1.8 MiB |
   | 1 TiB | 262 K | 7.2 MiB |
   | 4 TiB | 1048 K | 29 MiB |

   4 KiB 随机写在 256 MiB 卷上约 0.84 ms/次，可以接受；到 TiB 级别，每次写
   都要重写几 MB 元数据，会明显变慢。

   上表是**最坏情况**：快照里每个 chunk 都带脏块位图。实际卷通常小得多——
   1 TiB 卷上跑完 `mkfs.ext4` 之后元数据是 **16 KiB**，再多写几个块是 17 KiB，
   单次 4 KiB 写仍不到 1 ms。开销跟"卷碰过多少个对象"成正比，不是跟卷大小。
   真要在 TiB 卷上做大范围随机写，元数据会涨到 MiB 级，那时才需要增量格式。

   我试过把它改成按时间节流，但**节流和崩溃安全直接冲突**：写入后若进程被
   `kill -9`，脏块记录就丢了，数据会停留在旧对象上。现在选择的是安全——每次
   写入都落盘。真要解决需要增量/日志式元数据，那是另一个量级的改动。
6. **`discard` 只是丢弃缓存**：对象存储里没有"释放空间"的操作，空间靠新对象替换。
7. **一个卷只能被一个进程使用**。`MetaStore` 是 trait，接口上多客户端是可能的，
   但先要有一套租约（谁能写、租约过期怎么办、两个 writer 的网络分区怎么收敛）。
   在没有它之前，两个进程同时写同一个卷会互相覆盖元数据并损坏数据。用
   `blobdev run` 开同一个卷的第二个实例会被 `.lock` 挡住，换台机器就没有这层
   保护了——所以这一条是靠纪律而不是靠代码保证的。

## License

Apache-2.0
