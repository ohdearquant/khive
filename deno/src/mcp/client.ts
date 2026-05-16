/**
 * MCP client wrapper around @modelcontextprotocol/sdk.
 *
 * Spawns `khive-mcp` as a child process and exposes its tools over stdio.
 * Used by the research orchestration layer to call into the Rust runtime.
 */

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

export class KhiveMcpClient {
  private client: Client;
  private transport: StdioClientTransport;

  private constructor(client: Client, transport: StdioClientTransport) {
    this.client = client;
    this.transport = transport;
  }

  static async connect(command?: string): Promise<KhiveMcpClient> {
    const cmd = command ?? Deno.env.get("KHIVE_MCP_COMMAND") ?? "khive-mcp";
    const [bin, ...args] = cmd.split(" ");

    const transport = new StdioClientTransport({
      command: bin,
      args,
    });

    const client = new Client(
      {
        name: "khive-server",
        version: "0.1.0",
      },
      {
        capabilities: {},
      },
    );

    await client.connect(transport);
    return new KhiveMcpClient(client, transport);
  }

  async callTool(name: string, args: Record<string, unknown>): Promise<unknown> {
    const result = await this.client.callTool({ name, arguments: args });
    return result;
  }

  async listTools(): Promise<unknown> {
    return await this.client.listTools();
  }

  async close(): Promise<void> {
    await this.client.close();
  }
}
