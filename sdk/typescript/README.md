# @a3s-lab/box

TypeScript SDK for A3S Box — embedded MicroVM sandboxes.

## Install

```bash
npm install @a3s-lab/box
```

## Quick Start

```ts
import { BoxSdk } from "@a3s-lab/box";

const sdk = new BoxSdk();
const sandbox = await sdk.create({ image: "alpine:latest" });

const result = await sandbox.exec("echo", "hello");
console.log(result.stdout);        // "hello\n"
console.log(result.metrics.durationMs); // execution time

await sandbox.stop();
```

## Streaming Exec

```ts
for await (const event of await sandbox.execStream("make", "build")) {
  if (event.type === "chunk") {
    process.stdout.write(event.chunk.data);
  } else {
    console.log(`Exit: ${event.exit.exitCode}`);
  }
}
```

## File Transfer

```ts
await sandbox.upload(Buffer.from("hello world"), "/tmp/test.txt");
const data = await sandbox.download("/tmp/test.txt");
```

## Exec with Options

```ts
const result = await sandbox.exec(["python", "-c", "print('hi')"], {
  env: ["API_KEY=secret"],
  workingDir: "/app",
  user: "nobody",
});
```

## Options

```ts
const sandbox = await sdk.create({
  image: "python:3.12-slim",
  cpus: 4,
  memoryMb: 2048,
  env: { API_KEY: "secret" },
  mounts: [{ hostPath: "/local/data", guestPath: "/data", readonly: true }],
  portForwards: [{ guestPort: 8080, hostPort: 3000 }],
  workspace: { name: "my-project" },
});
```

## License

MIT
