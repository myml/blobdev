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
cargo test                      # 单元 + 集成测试，不需要网络和内核支持
scripts/e2e-dir.sh              # 端到端：真块设备 + mkfs + 重启校验（需要 ublk 和 root）
scripts/webdav_server.py DIR    # 本地 WebDAV 测试服务端（纯标准库）
```

端到端脚本验证：设备容量、mkfs/mount、写 120 MiB、缓存读回一致、
flush 后对象存储出现 chunk 对象、**完全重启后从对象存储重读并比对 sha256**。

## 已知限制

1. **元数据只有内存和本地文件**。多客户端共享一个卷需要换成数据库/Redis
   后端（`MetaStore` 是 trait，接口已经留好）。
2. **S3 写放大 2048×**：改一个 4 KiB 块要传 8 MiB。减小 chunk 能减轻，
   但会增加请求数；没有银弹。
3. **读未命中 = 整个 chunk**：读一个块要拉 8 MiB。顺序读没问题，
   随机小读很不划算，后续可以加 range 读。
4. **单队列、同步 IO**：正确性优先，不是吞吐最优形态。
5. **`discard` 是空操作**：chunk 整体替换时空间自然回收。

## License

Apache-2.0
