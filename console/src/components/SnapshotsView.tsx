// 快照树:从 snap.tree 节点数组自建 DAG,分层布局 SVG 渲染。
// 必须表达「回退后旧时间线仍在、fork 是树状分出」(snapshots.md §4)。
import { useMemo, useState } from 'react'
import { client, msg, useStore } from '../state/store'
import type { SnapNode } from '../protocol/types'
import { branchColor, layoutTree, NODE_H, NODE_W } from '../util/treelayout'
import { fmtDateTime, fmtBytes, shortId } from '../util/format'

const KIND_LABEL: Record<string, string> = {
  disk: '磁盘',
  memory: '内存',
  hybrid: '混合',
}

export function SnapshotsView() {
  const tree = useStore((s) => s.snapTree)
  const snapsLoading = useStore((s) => s.snapsLoading)
  const refreshSnaps = useStore((s) => s.refreshSnaps)
  const showToast = useStore((s) => s.showToast)

  const [selected, setSelected] = useState<string | null>(null)
  const [forkOpen, setForkOpen] = useState(false)
  const [forkName, setForkName] = useState('')
  const [busy, setBusy] = useState(false)

  const layout = useMemo(() => (tree ? layoutTree(tree.nodes) : null), [tree])
  const selNode = tree?.nodes.find((n) => n.id === selected) ?? null

  async function restoreInplace(node: SnapNode) {
    setBusy(true)
    try {
      const r = await client.snapRestore(node.id, 'inplace')
      showToast(`已回退到 ${shortId(r.restoredTo)}`)
      await refreshSnaps()
    } catch (err) {
      showToast(`回退失败:${msg(err)}`, 'error')
    } finally {
      setBusy(false)
    }
  }

  async function restoreFork() {
    if (!selNode || !forkName.trim()) return
    setBusy(true)
    try {
      const r = await client.snapRestore(selNode.id, 'fork', forkName.trim())
      showToast(`已 fork 出新分支 ${r.newBranch ?? forkName.trim()}`)
      setForkOpen(false)
      setForkName('')
      await refreshSnaps()
    } catch (err) {
      showToast(`fork 失败:${msg(err)}`, 'error')
    } finally {
      setBusy(false)
    }
  }

  return (
    <section className="col col-snaps">
      <div className="col-head">
        快照树
        {tree && (
          <span className="col-head-meta">
            {tree.branch} · head {shortId(tree.head)}
          </span>
        )}
        <button className="btn btn-small" onClick={() => void refreshSnaps()}>
          刷新
        </button>
      </div>
      <div className="col-body snap-body">
        {!tree && <div className="empty">{snapsLoading ? '加载中…' : '未连接或暂无快照'}</div>}
        {layout && tree && (
          <div className="tree-scroll">
            <svg width={layout.width} height={layout.height} className="tree-svg">
              {layout.edges.map((e, i) => (
                <path
                  key={i}
                  d={edgePath(e.from.x, e.from.y, e.to.x, e.to.y)}
                  stroke={branchColor(e.to.node.branch)}
                  strokeWidth={1.5}
                  fill="none"
                  opacity={0.7}
                />
              ))}
              {layout.placed.map((p) => {
                const n = p.node
                const isHead = tree.head === n.id
                const isSel = selected === n.id
                return (
                  <g
                    key={n.id}
                    transform={`translate(${p.x},${p.y})`}
                    className={`snap-node kind-${n.kind} ${isSel ? 'selected' : ''} ${isHead ? 'head' : ''}`}
                    onClick={() => setSelected(n.id)}
                  >
                    <rect
                      width={NODE_W}
                      height={NODE_H}
                      rx={6}
                      className="node-box"
                      style={{ stroke: isHead ? 'var(--accent)' : branchColor(n.branch) }}
                    />
                    <text x={10} y={17} className="node-label">
                      {(n.label || shortId(n.id)).slice(0, 18)}
                    </text>
                    <text x={10} y={33} className="node-branch" fill={branchColor(n.branch)}>
                      {n.branch}
                      {n.auto ? ' · 自动' : ''}
                      {n.origin ? ' · 远端' : ''}
                    </text>
                    <text x={10} y={48} className="node-meta">
                      {KIND_LABEL[n.kind] ?? n.kind} · {fmtDateTime(n.createdAt)} · {fmtBytes(n.sizeBytes)}
                    </text>
                    {isHead && <text x={NODE_W - 8} y={17} className="node-head-mark">head</text>}
                  </g>
                )
              })}
            </svg>
          </div>
        )}
        {selNode && (
          <div className="snap-detail">
            <div className="snap-detail-line">
              <strong>{selNode.label || '未命名'}</strong>
              <span className="mono">{selNode.id}</span>
            </div>
            <div className="snap-detail-line">
              分支 {selNode.branch} · {KIND_LABEL[selNode.kind]} · {fmtBytes(selNode.sizeBytes)} ·{' '}
              {selNode.chunks} 块 · {fmtDateTime(selNode.createdAt)}
              {selNode.origin ? ` · 来自 ${selNode.origin}` : ''}
            </div>
            {selNode.kernel && (
              <div className="snap-detail-line mono">
                {selNode.kernel} · {selNode.arch}
              </div>
            )}
            <div className="snap-detail-actions">
              <button
                className="btn btn-primary btn-small"
                disabled={busy || tree?.head === selNode.id}
                onClick={() => void restoreInplace(selNode)}
                title="工作区回退到该节点;之后的快照挂在其下,旧时间线仍在"
              >
                回退到此节点
              </button>
              <button
                className="btn btn-small"
                disabled={busy}
                onClick={() => setForkOpen(true)}
                title="从该节点分出新分支继续工作"
              >
                fork 新分支…
              </button>
            </div>
          </div>
        )}
        {forkOpen && selNode && (
          <div className="overlay" onClick={() => setForkOpen(false)}>
            <div className="modal modal-small" onClick={(e) => e.stopPropagation()}>
              <h2>fork 新分支</h2>
              <p className="hint">
                从 <span className="mono">{shortId(selNode.id)}</span>(
                {selNode.label || '未命名'})分出新分支。分支名冲突会自动加 -2、-3。
              </p>
              <label className="field">
                <span>分支名</span>
                <input
                  value={forkName}
                  onChange={(e) => setForkName(e.target.value)}
                  placeholder="如:调参-激进"
                  autoFocus
                />
              </label>
              <div className="modal-actions">
                <button className="btn" onClick={() => setForkOpen(false)}>
                  取消
                </button>
                <button
                  className="btn btn-primary"
                  disabled={!forkName.trim() || busy}
                  onClick={() => void restoreFork()}
                >
                  创建并切换
                </button>
              </div>
            </div>
          </div>
        )}
      </div>
    </section>
  )
}

// 父节点底部中点 → 子节点顶部中点的三次贝塞尔
function edgePath(fx: number, fy: number, tx: number, ty: number): string {
  const x1 = fx + NODE_W / 2
  const y1 = fy + NODE_H
  const x2 = tx + NODE_W / 2
  const y2 = ty
  const mid = (y1 + y2) / 2
  return `M ${x1} ${y1} C ${x1} ${mid}, ${x2} ${mid}, ${x2} ${y2}`
}
