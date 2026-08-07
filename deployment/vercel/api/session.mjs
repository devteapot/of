const SPACETIME_HOST = process.env.SPACETIME_HOST ?? "https://maincloud.spacetimedb.com";

export default async function handler(request, response) {
  if (request.method !== "POST") {
    response.setHeader("Allow", "POST");
    return response.status(405).json({ error: "method_not_allowed" });
  }

  const upstream = await fetch(`${SPACETIME_HOST}/v1/identity`, { method: "POST" });
  const body = await upstream.text();
  response.setHeader("Cache-Control", "no-store");
  response.setHeader("Content-Type", "application/json");
  return response.status(upstream.status).send(body);
}
