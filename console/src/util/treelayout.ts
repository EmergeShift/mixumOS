// 快照树分层布局(自研,不用图形库)。
// 树语义见 docs/snapshots.md §4:节点带 parent,回退后旧时间线仍在,
// 同一父可有多个孩子(genesis 之外也可能多分支汇不到一起,是纯森林 DAG)。
// 布局:depth = 到根的最长路径;层内按(分支, 时间)排序;x = 层内序号,y = 层号。
import type { SnapNode } from '../protocol/types'

export const NODE_W = 148
export const NODE_H = 56
export const COL_W = 176
export const ROW_H = 104
export const PAD = 28

export interface PlacedNode {
  node: SnapNode
  x: number // 盒子左上角
  y: number
}

export interface Layout {
  placed: PlacedNode[]
  edges: { from: PlacedNode; to: PlacedNode }[]
  width: number
  height: number
}

export function layoutTree(nodes: SnapNode[]): Layout {
  const byId = new Map(nodes.map((n) => [n.id, n]))
  const children = new Map<string, SnapNode[]>()
  const roots: SnapNode[] = []
  for (const n of nodes) {
    if (n.parent && byId.has(n.parent)) {
      const list = children.get(n.parent) ?? []
      list.push(n)
      children.set(n.parent, list)
    } else {
      roots.push(n)
    }
  }

  // depth = 最长路径(自顶向下记忆化;树无环,parent 引用已确保只指向存在节点)
  const depth = new Map<string, number>()
  const depthOf = (n: SnapNode): number => {
    const hit = depth.get(n.id)
    if (hit !== undefined) return hit
    const d = n.parent && byId.has(n.parent) ? depthOf(byId.get(n.parent)!) + 1 : 0
    depth.set(n.id, d)
    return d
  }
  for (const n of nodes) depthOf(n)

  // 层内排序:同分支按创建时间,不同分支按首个节点时间(保持分支块相邻)
  const layers = new Map<number, SnapNode[]>()
  for (const n of nodes) {
    const d = depth.get(n.id) ?? 0
    const list = layers.get(d) ?? []
    list.push(n)
    layers.set(d, list)
  }
  const branchStart = new Map<string, number>()
  for (const n of nodes) {
    const t = branchStart.get(n.branch)
    if (t === undefined || n.createdAt < t) branchStart.set(n.branch, n.createdAt)
  }
  const maxDepth = Math.max(-1, ...layers.keys())
  for (let d = 0; d <= maxDepth; d++) {
    layers.get(d)?.sort(
      (a, b) =>
        (branchStart.get(a.branch) ?? 0) - (branchStart.get(b.branch) ?? 0) ||
        a.createdAt - b.createdAt
    )
  }

  const placed: PlacedNode[] = []
  for (let d = 0; d <= maxDepth; d++) {
    const list = layers.get(d) ?? []
    list.forEach((n, i) => {
      placed.push({ node: n, x: PAD + i * COL_W, y: PAD + d * ROW_H })
    })
  }
  const pos = new Map(placed.map((p) => [p.node.id, p]))
  const edges: { from: PlacedNode; to: PlacedNode }[] = []
  for (const p of placed) {
    const from = p.node.parent ? pos.get(p.node.parent) : undefined
    if (from) edges.push({ from, to: p })
  }

  const cols = Math.max(1, ...[...layers.values()].map((l) => l.length))
  return {
    placed,
    edges,
    width: PAD * 2 + cols * COL_W,
    height: PAD * 2 + (maxDepth + 1) * ROW_H,
  }
}

// 克制的分支配色(6 色循环,低饱和)
const BRANCH_COLORS = ['#5b7d9e', '#7d9e5b', '#9e5b7d', '#9e8b5b', '#5b9e8b', '#83789f']
export function branchColor(branch: string): string {
  let h = 0
  for (let i = 0; i < branch.length; i++) h = (h * 31 + branch.charCodeAt(i)) >>> 0
  return BRANCH_COLORS[h % BRANCH_COLORS.length]
}
