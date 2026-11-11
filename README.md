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
<bucket>/<prefix>/<volume>/c/<chunk-id>    chunk 数据对象（最后一个可能不满）
```

卷被切成固定大小的 chunk（默认 8 MiB）。读时按块号定位 chunk，缓存未命中就拉整个
chunk；写时只改内存工作副本并记录脏块。

### 写策略取决于后端能力

`ObjectStore::caps()` 声明后端能做什么，卷据此选策略：

| 后端 | `partial_write` | 上传方式 | 4 KiB 写的实际流量 |
|---|---|---|---|
| S3 | 否 | 整个 chunk 替换 | 8 MiB（放大 2048×） |
| WebDAV | 是 | 只重写脏块区间 | 4 KiB（约 1×） |
| 本地目录 | 是 | 只重写脏块区间 | 4 KiB |

这是选择后端时最该看的一条：**S3 的对象不可变，改一个块就要重传整个 chunk；
WebDAV 支持 `Content-Range`，可以只写改动的部分。**

### 读缓存

读未命中时，按请求大小分两种策略：

| 情况 | 行为 |
|---|---|
| 读覆盖 chunk 的 ≥ `--whole-chunk-pct`（默认 25%） | 拉整个 chunk 并缓存（顺序读最优） |
| 更小的读 | **只拉需要的字节区间**（range read） |

没有 range read 的话，S3 上读一个 4 KiB 块要传 8 MiB，放大 2048 倍。

**两级缓存**：

- **内存 LRU**（`--read-chunks`，默认 64 个 chunk）：热数据，零 IO
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
```

`--create-size` 只在卷不存在时生效，之后从元数据读几何信息。

挂上之后就是普通块设备：

```bash
mkfs.ext4 /dev/ublkb0
mount /dev/ublkb0 /mnt
cp bigfile /mnt/
umount /mnt          # 触发 flush，所有脏 chunk 上传
```

## 一致性

- **对象存储是权威数据。** 本地缓存丢了只影响速度。
- 元数据在每次写后原子落盘（临时文件 + rename + CRC）。进程崩溃后重启，
  会从元数据恢复“哪些块还没上传”，下次 flush 补齐。
- S3 的 `PutObject` 是原子的，chunk 不会出现“写了一半”的中间状态。
- 元数据文件丢失 = 需要重建卷（chunk 对象还在，但脏块位图没了）。

## 测试

```bash
cargo test                      # 51 单元 + 4 集成测试，不需要网络和内核支持
scripts/e2e-dir.sh              # 端到端（目录后端）
scripts/e2e-s3.sh               # 端到端（S3，需先准备服务端）
scripts/e2e-webdav.sh           # 端到端（WebDAV）
scripts/webdav_server.py DIR    # 本地 WebDAV 测试服务端（纯标准库）
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
2. **S3 写放大 2048×**：改一个 4 KiB 块要传 8 MiB。减小 chunk 能减轻，
   但会增加请求数；没有银弹。
3. **磁盘缓存不做跨卷/跨进程共享**：每个卷一个目录，多进程同时用同一卷会互相干扰。
4. **单队列、同步 IO**：正确性优先，不是吞吐最优形态。
5. **`discard` 是空操作**：chunk 整体替换时空间自然回收。

## License

Apache-2.0
