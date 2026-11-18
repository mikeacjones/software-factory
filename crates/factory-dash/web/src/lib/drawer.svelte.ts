// The run drawer: one agent's run, read as it is written, opened from
// anywhere on the page.

import type { Run } from './types';

export interface Opened {
  where: Run['where'];
  id: string;
  file: string;
  files: string[];
  title: string;
  sub: string;
}

export const drawer = $state({ run: null as Opened | null });

export function openRun(run: Pick<Run, 'where' | 'id' | 'file'>, files: string[] | undefined, title: string, sub: string) {
  drawer.run = { where: run.where, id: run.id, file: run.file, files: files?.length ? files : [run.file], title, sub };
}

export function closeRun() {
  drawer.run = null;
}
