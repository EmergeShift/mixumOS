// 键盘事件 → 协议 keysym(XKB 名)+ 修饰键。协议 §4.4 输入事件表。
const NAMED: Record<string, string> = {
  Enter: 'Return',
  Escape: 'Escape',
  Backspace: 'BackSpace',
  Tab: 'Tab',
  Delete: 'Delete',
  ArrowUp: 'Up',
  ArrowDown: 'Down',
  ArrowLeft: 'Left',
  ArrowRight: 'Right',
  Home: 'Home',
  End: 'End',
  PageUp: 'Page_Up',
  PageDown: 'Page_Down',
  Insert: 'Insert',
  ' ': 'space',
}

export function toKeysym(e: KeyboardEvent): string | null {
  if (e.key.length === 1) return NAMED[e.key] ?? e.key
  if (/^F\d{1,2}$/.test(e.key)) return e.key
  return NAMED[e.key] ?? null
}

export function modsOf(e: KeyboardEvent): string[] {
  const mods: string[] = []
  if (e.ctrlKey) mods.push('ctrl')
  if (e.altKey) mods.push('alt')
  if (e.shiftKey) mods.push('shift')
  if (e.metaKey) mods.push('logo')
  return mods
}
