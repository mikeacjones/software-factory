// Where the owner has put things on a project's map, kept in this browser
// only. A card the owner moved stays where it was put while the picture
// refreshes; Tidy forgets it all and lays the map out again.

export type Placed = Record<string, { x: number; y: number }>;

const key = (project: string) => `factory.map.v1.${project}`;

export function load(project: string): Placed {
  try {
    const text = localStorage.getItem(key(project));
    const read = text ? JSON.parse(text) : {};
    return read && typeof read === 'object' ? read : {};
  } catch {
    return {};
  }
}

export function save(project: string, placed: Placed) {
  try {
    if (Object.keys(placed).length) localStorage.setItem(key(project), JSON.stringify(placed));
    else localStorage.removeItem(key(project));
  } catch {
    // A private window keeps nothing; the map still works.
  }
}

export function loadChoice<T extends string>(name: string, allowed: readonly T[], fallback: T): T {
  try {
    const value = localStorage.getItem(`factory.${name}`) as T | null;
    return value && allowed.includes(value) ? value : fallback;
  } catch {
    return fallback;
  }
}

export function saveChoice(name: string, value: string) {
  try {
    localStorage.setItem(`factory.${name}`, value);
  } catch {
    // As above.
  }
}

const openKey = (project: string) => `factory.open.v1.${project}`;

/** Which cards the owner opened or folded by hand, by project. */
export function loadOpen(project: string): Record<string, boolean> {
  try {
    const read = JSON.parse(localStorage.getItem(openKey(project)) || '{}');
    return read && typeof read === 'object' ? read : {};
  } catch {
    return {};
  }
}

export function saveOpen(project: string, open: Record<string, boolean>) {
  try {
    localStorage.setItem(openKey(project), JSON.stringify(open));
  } catch {
    // As above.
  }
}
