import { readFile } from "node:fs/promises";

const SPACETIME_HOST = process.env.SPACETIME_HOST ?? "https://maincloud.spacetimedb.com";
const LOBBY_DATABASE = process.env.LOBBY_DATABASE ?? "of-lobby";
const MATCH_WASM =
  process.env.MATCH_WASM_PATH ?? new URL("../assets/match_module.wasm", import.meta.url);
const PRESET_NAMES = ["small", "medium", "large"];
const STATUS_NAMES = ["pending", "provisioning", "open", "full", "failed", "cancelled"];

function bearer(request) {
  const value = request.headers.authorization ?? "";
  if (!value.startsWith("Bearer ") || value.length <= 7) {
    throw Object.assign(new Error("A lobby session is required"), { statusCode: 401 });
  }
  return value.slice(7);
}

function serviceToken() {
  const value = process.env.SPACETIMEDB_TOKEN?.trim();
  if (!value) {
    throw Object.assign(new Error("Match provisioning is not configured"), { statusCode: 503 });
  }
  return value;
}

function enumName(value, names) {
  if (Array.isArray(value) && Number.isInteger(value[0])) return names[value[0]] ?? "unknown";
  if (value && typeof value === "object") return Object.keys(value)[0] ?? "unknown";
  return String(value).toLowerCase();
}

async function upstream(path, { method = "GET", token, body, contentType } = {}) {
  const result = await fetch(`${SPACETIME_HOST}${path}`, {
    method,
    headers: {
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      ...(contentType ? { "Content-Type": contentType } : {}),
    },
    body,
  });
  const text = await result.text();
  if (!result.ok) {
    throw Object.assign(new Error(text || `SpacetimeDB returned ${result.status}`), {
      statusCode: result.status,
    });
  }
  if (/PermissionDenied|HostExecutionError|ReducerError|DatabaseError/.test(text)) {
    throw new Error(text);
  }
  return text;
}

async function callReducer(database, reducer, args, token) {
  return upstream(`/v1/database/${encodeURIComponent(database)}/call/${reducer}`, {
    method: "POST",
    token,
    contentType: "application/json",
    body: JSON.stringify(args),
  });
}

async function query(sql) {
  const text = await upstream(`/v1/database/${LOBBY_DATABASE}/sql`, {
    method: "POST",
    contentType: "text/plain",
    body: sql,
  });
  return JSON.parse(text)?.[0]?.rows ?? [];
}

async function listLobbies() {
  const [lobbyRows, memberRows] = await Promise.all([
    query("SELECT * FROM lobby"),
    query("SELECT * FROM lobby_member"),
  ]);
  const members = new Map();
  for (const row of memberRows) {
    const list = members.get(row[1]) ?? [];
    list.push({ identity: String(row[2]), displayName: row[3] });
    members.set(row[1], list);
  }
  return lobbyRows
    .map((row) => ({
      lobbyId: row[0],
      creatorIdentity: String(row[1]),
      mapPreset: enumName(row[2], PRESET_NAMES),
      playerCount: row[3],
      memberCount: row[4],
      status: enumName(row[5], STATUS_NAMES),
      matchDatabase: row[6],
      failureReason: row[7],
      createdAtUs: row[8],
      updatedAtUs: row[9],
      members: members.get(row[0]) ?? [],
    }))
    .sort((left, right) => Number(right.createdAtUs) - Number(left.createdAtUs));
}

function matchPreset(lobbyPreset) {
  return {
    small: { dev64: {} },
    medium: { playtest128: {} },
    large: { validation192: {} },
  }[lobbyPreset];
}

async function provision(lobby) {
  const token = serviceToken();
  const database = `of-match-${lobby.lobbyId}`;
  await callReducer(LOBBY_DATABASE, "begin_provision", [lobby.lobbyId], token);
  try {
    const wasm = await readFile(MATCH_WASM);
    await upstream(`/v1/database/${database}?clear=false`, {
      method: "PUT",
      token,
      contentType: "application/wasm",
      body: wasm,
    });
    try {
      await callReducer(
        database,
        "configure_match",
        [matchPreset(lobby.mapPreset), lobby.playerCount],
        token,
      );
    } catch (error) {
      if (!String(error.message).includes("configuration is already locked")) throw error;
    }
    await callReducer(LOBBY_DATABASE, "complete_provision", [lobby.lobbyId, database], token);
    return database;
  } catch (error) {
    await callReducer(
      LOBBY_DATABASE,
      "fail_provision",
      [lobby.lobbyId, String(error.message).slice(0, 160)],
      token,
    ).catch(() => {});
    throw error;
  }
}

async function jsonBody(request) {
  if (request.body && typeof request.body === "object") return request.body;
  if (typeof request.body === "string") return JSON.parse(request.body);
  return {};
}

export default async function handler(request, response) {
  response.setHeader("Cache-Control", "no-store");
  try {
    if (request.method === "GET") {
      return response.status(200).json({ lobbies: await listLobbies() });
    }
    if (request.method !== "POST") {
      response.setHeader("Allow", "GET, POST");
      return response.status(405).json({ error: "method_not_allowed" });
    }

    const token = bearer(request);
    const body = await jsonBody(request);
    if (body.action === "create") {
      const lobbyId = String(body.lobbyId ?? "");
      const mapPreset = String(body.mapPreset ?? "small");
      const playerCount = Number(body.playerCount);
      const displayName = String(body.displayName ?? "");
      const lobbyPreset = { small: { small: {} }, medium: { medium: {} }, large: { large: {} } }[
        mapPreset
      ];
      if (!lobbyPreset || !Number.isInteger(playerCount)) {
        return response.status(400).json({ error: "invalid_lobby_parameters" });
      }
      // Require the orchestrator token before create_lobby so a misconfigured
      // deploy cannot leave an orphan Pending row that join cannot use.
      serviceToken();
      await callReducer(
        LOBBY_DATABASE,
        "create_lobby",
        [lobbyId, lobbyPreset, playerCount, displayName],
        token,
      );
      let lobby = (await listLobbies()).find((candidate) => candidate.lobbyId === lobbyId);
      if (!lobby) throw new Error("Lobby creation committed without a visible lobby row");
      if (!lobby.matchDatabase) await provision(lobby);
      lobby = (await listLobbies()).find((candidate) => candidate.lobbyId === lobbyId);
      return response.status(201).json({ lobby });
    }

    if (body.action === "join") {
      const lobbyId = String(body.lobbyId ?? "");
      const displayName = String(body.displayName ?? "");
      await callReducer(LOBBY_DATABASE, "join_lobby", [lobbyId, displayName], token);
      const lobby = (await listLobbies()).find((candidate) => candidate.lobbyId === lobbyId);
      if (!lobby?.matchDatabase) throw new Error("Lobby has no provisioned match database");
      return response.status(200).json({ lobby });
    }

    if (body.action === "leave") {
      const lobbyId = String(body.lobbyId ?? "");
      await callReducer(LOBBY_DATABASE, "leave_lobby", [lobbyId], token);
      const lobby = (await listLobbies()).find((candidate) => candidate.lobbyId === lobbyId);
      return response.status(200).json({ lobby: lobby ?? null });
    }

    return response.status(400).json({ error: "unknown_action" });
  } catch (error) {
    const status = Number(error.statusCode) || 500;
    return response.status(status).json({ error: error.message || "unexpected_error" });
  }
}
