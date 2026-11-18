// Whether a node or edge of a graph made afresh is the one already drawn,
// so the graph keeps the one it has: Svelte Flow then neither measures it
// again nor draws it again, and only what changed moves on the map.

/** What Svelte Flow writes onto a node itself, not what the page made. */
const OWN = new Set(['measured', 'selected', 'dragging', 'resizing', 'internals']);

function same(a: unknown, b: unknown, top: boolean): boolean {
  if (a === b) return true;
  // A handler is made afresh with every graph and does the same thing.
  if (typeof a === 'function' && typeof b === 'function') return true;
  if (typeof a !== 'object' || typeof b !== 'object' || !a || !b) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a)) {
    const other = b as unknown[];
    return a.length === other.length && a.every((value, index) => same(value, other[index], false));
  }
  const left = a as Record<string, unknown>;
  const right = b as Record<string, unknown>;
  const keys = (of: Record<string, unknown>) => Object.keys(of).filter((key) => of[key] !== undefined && !(top && OWN.has(key)));
  const mine = keys(left);
  return mine.length === keys(right).length && mine.every((key) => same(left[key], right[key], false));
}

/** The fresh list, with each item that is the same as one before (by id)
 * replaced by that one. The list itself is the one before if nothing
 * changed at all. */
export function keep<T extends { id: string }>(before: T[], fresh: T[]): T[] {
  const old = new Map(before.map((one) => [one.id, one]));
  let changed = before.length !== fresh.length;
  const kept = fresh.map((one, index) => {
    const was = old.get(one.id);
    if (was && same(was, one, true)) {
      if (before[index] !== was) changed = true;
      return was;
    }
    changed = true;
    return one;
  });
  return changed ? kept : before;
}
