export type Theme = 'light' | 'dark' | 'system';

export interface AppConfig {
  port: number;
  exclude_patterns: string[];
  last_src: string | null;
  last_dst: string | null;
  theme: Theme;
}

// The user-editable part of AppConfig. last_src/last_dst are recorded by the
// server on every preview and are not accepted here: sending the whole config
// back used to regress them to whatever the page loaded with.
export interface ConfigPatch {
  port?: number;
  exclude_patterns?: string[];
  theme?: Theme;
}

export type SyncStatus =
  | 'idle'
  | 'previewing'
  | 'running'
  | 'paused'
  | 'done'
  | 'cancelled';

export type Badge = '+' | '–' | '→' | '↻' | '⇢' | '~' | '!';

export type LogLevel = 'info' | 'warning' | 'error';

export interface LogEntry {
  level: LogLevel;
  message: string;
  run: number;
}

export interface OpEntry {
  kind: 'copy' | 'overwrite' | 'move' | 'delete' | 'dir-rename' | 'case-rename' | 'symlink' | 'touch';
  rel_path: string;
  size: number;
  badge: Badge;
  hash?: string;
  from_path?: string;
}

export interface PlanSummary {
  copy_count: number;
  move_count: number;
  delete_count: number;
  overwrite_count: number;
  identical_count: number;
  symlink_count: number;
  total_bytes: number;
  total_ops: number;
  ops: OpEntry[];
}

export interface ProgressSnapshot {
  done_bytes: number;
  total_bytes: number;
  current_file: string | null;
  current_file_done: number;
  current_file_size: number;
  current_file_pct: number;
  current_dir: string | null;
  speed_mbps: number;
  elapsed_secs: number;
  eta_secs: number | null;
  ops_done: number;
  ops_total: number;
  status: SyncStatus;
}

export interface BrowseEntry {
  name: string;
  path: string;
  is_dir: boolean;
}

// WebSocket events from server
export type ScanProgressPhase =
  | 'walking_src'
  | 'walking_dst'
  | 'hashing'
  | 'planning';

export type WsEvent =
  | ({ type: 'progress_update' } & ProgressSnapshot)
  | { type: 'status_changed'; status: SyncStatus }
  | { type: 'error_occurred'; path: string; message: string }
  | { type: 'ops_completed'; rel_paths: string[] }
  | { type: 'scan_update'; side: 'src' | 'dst'; file_count: number }
  | { type: 'scan_progress'; phase: ScanProgressPhase; path: string | null }
  | { type: 'drive_mode'; hdd: boolean }
  | ({ type: 'plan_ready' } & PlanSummary)
  | { type: 'shutdown' }
  | ({ type: 'log_entry' } & LogEntry);
