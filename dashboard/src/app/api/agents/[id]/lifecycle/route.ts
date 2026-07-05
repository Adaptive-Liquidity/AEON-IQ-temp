import { NextRequest, NextResponse } from "next/server";
import { auth } from "@/auth";
import { backendUrl, mgmtHeaders } from "@/lib/backend";

type Ctx = { params: Promise<{ id: string }> };

function forbidden() {
  return NextResponse.json({ error: "Forbidden" }, { status: 403 });
}

export async function GET(req: NextRequest, { params }: Ctx) {
  const session = await auth();
  if (!session) return NextResponse.json({ error: "Unauthorized" }, { status: 401 });

  const { id } = await params;
  if (!session.user.isAdmin && id !== session.user.agentId) return forbidden();

  const { searchParams } = new URL(req.url);
  const qs = searchParams.toString();
  try {
    const url = backendUrl(`/api/v1/agents/${encodeURIComponent(id)}/lifecycle${qs ? `?${qs}` : ""}`);
    const res = await fetch(url, { cache: "no-store", headers: mgmtHeaders() });
    return NextResponse.json(await res.json(), { status: res.status });
  } catch (err) {
    return NextResponse.json({ error: String(err) }, { status: 502 });
  }
}
