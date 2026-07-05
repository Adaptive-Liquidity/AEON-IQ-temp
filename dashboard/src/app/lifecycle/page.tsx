import { auth } from "@/auth";
import LifecycleClient from "./client";

export default async function LifecyclePage() {
  const session = await auth();
  return (
    <LifecycleClient
      userAgentId={session?.user?.agentId ?? ""}
      isAdmin={session?.user?.isAdmin ?? false}
    />
  );
}
