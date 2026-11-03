import { ORG_A, SESSION, WS_1, loggedInRoutes } from "./helpers.tsx";
import type { Handler } from "./helpers.tsx";

export const ME = SESSION.user_id;
export const BOB = "00000000-0000-4000-8000-000000000002";
export const CARA = "00000000-0000-4000-8000-000000000003";
export const OWEN = "00000000-0000-4000-8000-000000000004";

export const member = (user_id: string, display_name: string, role: string) => ({
  user_id,
  display_name,
  email: `${display_name.toLowerCase()}@example.test`,
  role,
  joined_at: "2026-10-02T10:00:00Z",
});

export const ORG_MEMBERS = [
  member(ME, "Ada", "owner"),
  member(BOB, "Bob", "admin"),
  member(CARA, "Cara", "member"),
  member(OWEN, "Owen", "owner"),
];

export const PROJECT_1 = {
  id: "00000000-0000-4000-8000-0000000000c1",
  organization_id: ORG_A.id,
  workspace_id: WS_1.id,
  name: "Brand kit",
  version: 3,
  created_by: ME,
  created_at: "2026-10-03T10:00:00Z",
  updated_by: ME,
  updated_at: "2026-10-04T10:00:00Z",
  deleted_at: null,
};
export const PROJECT_2 = { ...PROJECT_1, id: "00000000-0000-4000-8000-0000000000c2", name: "Poster", version: 1 };

export const HEAD = "00000000-0000-4000-8000-0000000000e1";
export const FILE_1 = {
  id: "00000000-0000-4000-8000-0000000000d1",
  organization_id: ORG_A.id,
  workspace_id: WS_1.id,
  project_id: PROJECT_1.id,
  name: "logo.fig",
  app: "figcraft",
  format: "fig",
  size_bytes: 5_242_880,
  sha256: "a".repeat(64),
  head_version_id: HEAD,
  version: 2,
  created_by: ME,
  created_at: "2026-10-03T10:00:00Z",
  updated_by: ME,
  updated_at: "2026-10-04T10:00:00Z",
  deleted_at: null,
};
export const FILE_EMPTY = {
  ...FILE_1,
  id: "00000000-0000-4000-8000-0000000000d2",
  name: "draft.fig",
  size_bytes: 0,
  sha256: null,
  head_version_id: null,
};

export const version = (number: number, id: string, extra: Record<string, unknown> = {}) => ({
  id,
  file_id: FILE_1.id,
  number,
  size_bytes: 1000 * number,
  sha256: String(number).repeat(64).slice(0, 64),
  base_version_id: null,
  restored_from_version_id: null,
  upload_id: null,
  created_by: ME,
  pinned: false,
  created_at: "2026-10-04T10:00:00Z",
  ...extra,
});

export const page = <T,>(items: T[], next: string | null = null) => ({ body: { items, next_cursor: next } });

/** A healthy server with members, projects and one file, for the view tests. */
export function viewRoutes(overrides: Record<string, Handler> = {}): Record<string, Handler> {
  return loggedInRoutes({
    [`GET /organizations/${ORG_A.id}/members`]: page(ORG_MEMBERS),
    [`GET /workspaces/${WS_1.id}/members`]: page([
      { ...member(ME, "Ada", "admin") },
      { ...member(BOB, "Bob", "editor") },
    ]),
    [`GET /organizations/${ORG_A.id}/invitations`]: page([]),
    [`GET /workspaces/${WS_1.id}/projects`]: (call) =>
      call.query.get("deleted") === "true" ? page([]) : page([PROJECT_1, PROJECT_2]),
    [`GET /projects/${PROJECT_1.id}/files`]: (call) =>
      call.query.get("deleted") === "true" ? page([]) : page([FILE_1, FILE_EMPTY]),
    [`GET /files/${FILE_1.id}/versions`]: page([version(2, HEAD), version(1, "00000000-0000-4000-8000-0000000000e0")]),
    ...overrides,
  });
}
