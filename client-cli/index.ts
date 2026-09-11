#!/usr/bin/env node
//
// Caspar Client CLI (`caspar-client`)
// -----------------------------------
// A thin TypeScript/Node.js client for a Caspar node's signed binary action
// protocol (the "Caspar shell API"). Every command in this CLI maps directly
// to a Caspar shell action route (`/creatures/*`, `/programs/*`) — there is no
// dependency on any hosted backend, billing service or miniapp layer.
//
// It lets you:
//   * authenticate against a node (`login` / `logout`),
//   * manage creatures (identities/accounts) and send signals,
//   * create, deploy, run and manage programs (the deployable VM units), and
//   * scaffold ready-to-deploy VM project templates for all six Caspar
//     runtimes (`vm.init` / `vm.types`).
//
import tls from "tls";
import net from "net";
import crypto from "crypto";
import fs from "fs";
import path from "path";
import exec from "child_process";
import readline from "node:readline";
import JSONbig from "json-bigint";
import { WebSocket } from "ws";

const USER_ID_NOT_SET_ERR_CODE: number = 10;
const USER_ID_NOT_SET_ERR_MSG: string = "not authenticated, userId is not set";

// The seven VM runtimes a Caspar node ships with. `vm.init` scaffolds a
// deployable project for any of these keys.
const VM_RUNTIMES = [
  "wasm",
  "javascript",
  "docker",
  "fire",
  "elpian",
  "elpify",
  "modal",
] as const;
type VmRuntime = (typeof VM_RUNTIMES)[number];

class Caspar {
  port: number = 8077; // TCP action port (CLIENT_TCP_API_PORT)
  port2: number = 8076; // WebSocket action port (CLIENT_WS_API_PORT)
  host: string = "127.0.0.1";
  protocol: string = "ws";
  // Transport TLS. A Caspar node serves plaintext ws/tcp directly; TLS is
  // normally terminated by a front proxy (nginx). Set CASPAR_TLS=0 to connect
  // straight to a node with no proxy (plain ws:// / plain TCP).
  useTls: boolean = process.env.CASPAR_TLS !== "0";
  callbacks: { [key: string]: (packageId: number, obj: any) => void } = {};
  socket: tls.TLSSocket | net.Socket | undefined;
  websocket: WebSocket | undefined;
  received: Buffer = Buffer.from([]);
  observePhase: boolean = true;
  nextLength: number = 0;
  readBytes() {
    if (this.observePhase) {
      if (this.received.length >= 4) {
        this.nextLength = this.received.subarray(0, 4).readIntBE(0, 4);
        this.received = this.received.subarray(4);
        this.observePhase = false;
        this.readBytes();
      }
    } else {
      if (this.received.length >= this.nextLength) {
        let payload = this.received.subarray(0, this.nextLength);
        this.received = this.received.subarray(this.nextLength);
        this.observePhase = true;
        this.processPacket(payload);
        this.readBytes();
      }
    }
  }
  private async connectoToTlsServer() {
    return new Promise((resolve, reject) => {
      const insecure = process.env.CASPAR_INSECURE === "1";
      if (this.protocol === "tcp") {
        const onData = (data: Buffer) => {
          setTimeout(() => {
            this.received = Buffer.concat([this.received, data]);
            this.readBytes();
          });
        };
        const onError = (e: any) => console.log(e);
        const onClose = (e: any) => {
          console.log(e);
          this.connectoToTlsServer();
        };
        if (this.useTls) {
          const options: tls.ConnectionOptions = {
            host: this.host,
            port: this.port,
            servername: this.host,
            rejectUnauthorized: !insecure,
          };
          const s = tls.connect(options, () => {
            if (s.authorized) {
              console.log("✔ Tcp TLS connection authorized");
              this.authenticate();
            } else {
              console.log(
                "⚠ TLS connection not authorized:",
                s.authorizationError
              );
            }
            resolve(undefined);
          });
          this.socket = s;
          s.on("error", onError);
          s.on("close", onClose);
          s.on("data", onData);
        } else {
          // Plaintext TCP straight to the node (no proxy).
          const s = net.connect({ host: this.host, port: this.port }, () => {
            console.log("✔ Tcp (plaintext) connected");
            this.authenticate();
            resolve(undefined);
          });
          this.socket = s;
          s.on("error", onError);
          s.on("close", onClose);
          s.on("data", onData);
        }
      } else {
        const scheme = this.useTls ? "wss" : "ws";
        this.websocket = new WebSocket(`${scheme}://${this.host}:${this.port2}`, {
          rejectUnauthorized: !insecure,
        } as any);
        this.websocket.on("open", () => {
          console.log("✔ Ws TLS connection authorized");
          this.authenticate();
          resolve(undefined);
        });
        this.websocket.on("error", (e) => {
          console.log("error:", e);
        });
        this.websocket.on("close", (e) => {
          console.log("close", e);
          this.connectoToTlsServer();
        });
        this.websocket.on("message", (data) => {
          setTimeout(() => {
            this.received = Buffer.concat([this.received, data as Buffer]);
            this.readBytes();
          });
        });
      }
    });
  }
  // Async req/res over the signaling channel: `creatures.signal` registers a
  // resolver keyed by correlationId here, then waits for an inbound 0x01
  // packet (key "creatures/signal/result") carrying that same id.
  private pendingSignalResponses: {
    [correlationId: string]: (resp: any) => void;
  } = {};
  private processPacket(data: Buffer) {
    try {
      let pointer = 0;
      if (data.at(pointer) == 0x01) {
        // Asynchronous update / signal frame.
        pointer++;
        let keyLen = data.subarray(pointer, pointer + 4).readIntBE(0, 4);
        pointer += 4;
        let key = data.subarray(pointer, pointer + keyLen).toString();
        pointer += keyLen;
        let payload = data.subarray(pointer);
        let obj: any;
        try {
          obj = JSONbig.parse(payload.toString());
        } catch {
          obj = payload.toString();
        }
        if (key == "creatures/signal/result") {
          const cid =
            obj && typeof obj === "object" ? obj.correlationId : undefined;
          if (cid && this.pendingSignalResponses[cid]) {
            const resolve = this.pendingSignalResponses[cid];
            delete this.pendingSignalResponses[cid];
            resolve(obj);
          } else {
            console.log("[signal-result]", obj);
          }
        } else {
          console.log(key, obj);
        }
      } else if (data.at(pointer) == 0x02) {
        // Synchronous response frame.
        pointer++;
        let pidLen = data.subarray(pointer, pointer + 4).readIntBE(0, 4);
        pointer += 4;
        let packetId = data.subarray(pointer, pointer + pidLen).toString();

        pointer += pidLen;
        let resCode = data.subarray(pointer, pointer + 4).readIntBE(0, 4);
        pointer += 4;
        let payload = data.subarray(pointer).toString();
        let obj = JSONbig.parse(payload);
        let cb = this.callbacks[packetId];
        if (cb) cb(resCode, obj);
      }
    } catch (ex) {
      console.log(ex);
    }
    setTimeout(() => {
      // Flow-control ack expected by the node after each processed frame.
      if (this.protocol === "tcp") {
        this.socket?.write(Buffer.from([0x00, 0x00, 0x00, 0x01, 0x01]));
      } else {
        this.websocket?.send(Buffer.from([0x00, 0x00, 0x00, 0x01, 0x01]));
      }
    });
  }
  // RSA-PSS (salt length 32) signature over the exact payload bytes — the
  // node's primary signature scheme for signed action packets.
  private sign(b: Buffer) {
    if (this.privateKey) {
      const sign = crypto.sign(null, b, {
        key: this.privateKey,
        padding: crypto.constants.RSA_PKCS1_PSS_PADDING,
        saltLength: 32,
      });
      return sign.toString("base64");
    } else {
      return "";
    }
  }
  private intToBytes(x: number) {
    const bytes = Buffer.alloc(4);
    bytes.writeInt32BE(x);
    return bytes;
  }
  private stringToBytes(x: string) {
    const bytes = Buffer.from(x);
    return bytes;
  }
  // Frame body layout expected by the Caspar action router:
  // [sigLen][sig][uidLen][uid][pathLen][path][pidLen][pid][payload]
  // prefixed by a 4-byte big-endian body length.
  private createRequest(userId: string, path: string, obj: any) {
    let packetId = Math.random().toString().substring(2);
    let payload = this.stringToBytes(JSONbig.stringify(obj));
    let signature = this.stringToBytes(this.sign(payload));
    let uidBytes = this.stringToBytes(userId);
    let pidBytes = this.stringToBytes(packetId);
    let pathBytes = this.stringToBytes(path);
    let b = Buffer.concat([
      this.intToBytes(signature.length),
      signature,
      this.intToBytes(uidBytes.length),
      uidBytes,
      this.intToBytes(pathBytes.length),
      pathBytes,
      this.intToBytes(pidBytes.length),
      pidBytes,
      payload,
    ]);
    return {
      packetId: packetId,
      data: Buffer.concat([this.intToBytes(b.length), b]),
    };
  }
  private async sendRequest(
    userId: string,
    path: string,
    obj: any
  ): Promise<{ resCode: number; obj: any }> {
    return new Promise((resolve, reject) => {
      let data = this.createRequest(userId, path, obj);
      let to: NodeJS.Timeout;
      this.callbacks[data.packetId] = (resCode, obj) => {
        if (to) {
          clearTimeout(to);
        }
        resolve({ resCode, obj });
      };
      to = setTimeout(() => {
        resolve({ resCode: 20, obj: { message: "request timeout" } });
        clearTimeout(to);
      }, 360000);
      setTimeout(() => {
        if (this.protocol === "tcp") {
          this.socket?.write(data.data);
        } else {
          this.websocket?.send(data.data);
        }
      });
    });
  }
  private async sleep(ms: number) {
    return new Promise((resolve) => {
      setTimeout(() => {
        resolve(undefined);
      }, ms);
    });
  }
  private userId: string | undefined;
  private privateKey: string | undefined;
  private username: string | undefined;
  public constructor(proto = "ws", host?: string, port?: number) {
    this.protocol = proto;
    if (host) this.host = host;
    if (port) {
      if (proto === "tcp") {
        this.port = port;
      } else {
        this.port2 = port;
      }
    }
    if (!fs.existsSync("auth")) fs.mkdirSync("auth");
    if (!fs.existsSync("files")) fs.mkdirSync("files");
    if (
      fs.existsSync("auth/userId.txt") &&
      fs.existsSync("auth/privateKey.txt")
    ) {
      this.userId = fs.readFileSync("auth/userId.txt", { encoding: "utf-8" });
      let pk = fs.readFileSync("auth/privateKey.txt", { encoding: "utf-8" });
      this.privateKey = pk;
    }
  }
  public async connect() {
    await this.connectoToTlsServer();
    if (this.userId && this.privateKey) {
      let auth = await this.authenticateSession();
      console.log(auth.obj);
    }
  }
  public async connectTransport() {
    await this.connectoToTlsServer();
  }
  public hasCredentials(): boolean {
    return !!this.userId && !!this.privateKey;
  }
  public async authenticateSession(): Promise<{ resCode: number; obj: any }> {
    if (!this.userId || !this.privateKey) {
      return {
        resCode: USER_ID_NOT_SET_ERR_CODE,
        obj: { message: USER_ID_NOT_SET_ERR_MSG },
      };
    }
    const authRes = await this.authenticate();
    if (authRes.resCode !== 0) {
      return authRes;
    }
    const meRes = await this.creatures.me();
    if (meRes.resCode !== 0) {
      return meRes;
    }
    this.username = meRes.obj?.user?.username;
    return {
      resCode: 0,
      obj: { message: "authenticated", user: meRes.obj?.user },
    };
  }
  // Direct login against the node's `/creatures/login` action. The node issues
  // (or looks up) the account for `username`, treating `emailToken` as the
  // account email; it returns the userId and the account private key, which are
  // persisted under ./auth. Works against any Caspar node — no hosted identity
  // provider is involved.
  public async login(
    username: string,
    email?: string
  ): Promise<{ resCode: number; obj: any }> {
    const emailToken =
      email && email.includes("@") ? email : `${username}@dev.local`;
    const res = await this.sendRequest("", "/creatures/login", {
      username,
      emailToken,
      metadata: {
        public: {
          profile: { name: username },
        },
      },
    });
    if (res.resCode == 0) {
      this.userId = res.obj.user.id;
      this.privateKey = res.obj.privateKey;
      await Promise.all([
        new Promise((resolve) =>
          fs.writeFile(
            "auth/userId.txt",
            this.userId ?? "",
            { encoding: "utf-8" },
            () => resolve(undefined)
          )
        ),
        new Promise((resolve) =>
          fs.writeFile(
            "auth/privateKey.txt",
            this.privateKey ?? "",
            { encoding: "utf-8" },
            () => resolve(undefined)
          )
        ),
      ]);
      await this.authenticate();
      const me = await this.creatures.me();
      this.username =
        me.obj?.user?.username ?? me.obj?.creature?.username ?? username;
      console.log("Login successful");
      // The key is already persisted in auth/privateKey.txt. Returning the raw
      // login response makes the generic command printer echo that secret to
      // terminals and CI logs, defeating the point of the credential file.
      return {
        resCode: 0,
        obj: {
          message: "login successful",
          user: { id: this.userId, username: this.username },
        },
      };
    }
    return res;
  }
  public async authenticate(): Promise<{ resCode: number; obj: any }> {
    if (!this.userId) {
      return {
        resCode: USER_ID_NOT_SET_ERR_CODE,
        obj: { message: USER_ID_NOT_SET_ERR_MSG },
      };
    }
    return await this.sendRequest(this.userId, "/creatures/authenticate", {});
  }
  public logout() {
    if (fs.existsSync("auth/userId.txt")) fs.rmSync("auth/userId.txt");
    if (fs.existsSync("auth/privateKey.txt")) fs.rmSync("auth/privateKey.txt");
    if (!this.userId && !this.privateKey && !this.username) {
      return { resCode: 1, obj: { message: "user is not logged in" } };
    }
    this.userId = undefined;
    this.privateKey = undefined;
    this.username = undefined;
    return { resCode: 0, obj: { message: "user logged out" } };
  }
  public myUsername(): string {
    return this.username ?? "Caspar User";
  }
  public myPrivateKey(): string {
    if (this.privateKey) {
      let str = this.privateKey
        .toString()
        .slice("-----BEGIN RSA PRIVATE KEY-----\n".length);
      str = str.slice(
        0,
        str.length - "\n-----END RSA PRIVATE KEY-----\n".length
      );
      return str;
    } else {
      return "empty";
    }
  }
  public creatures = {
    get: async (userId: string): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      return await this.sendRequest(this.userId, "/creatures/get", {
        userId: userId,
      });
    },
    lockToken: async (
      amount: number,
      type: string,
      target: string
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      let res = await this.sendRequest(this.userId, "/creatures/lockToken", {
        amount: amount,
        type: type,
        target: target,
      });
      console.log();
      console.log(this.sign(Buffer.from(res.obj.tokenId)));
      console.log();
      return res;
    },
    consumeLock: async (
      lockId: string,
      type: string,
      amount: number
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      return await this.sendRequest(this.userId, "/creatures/consumeLock", {
        amount: amount,
        type: type,
        lockId: lockId,
        signature: this.sign(Buffer.from(lockId)),
        userId: this.userId,
      });
    },
    me: async (): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      return await this.sendRequest(this.userId, "/creatures/get", {
        userId: this.userId,
      });
    },
    list: async (
      offset: number,
      count: number
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      return await this.sendRequest(this.userId, "/creatures/list", {
        offset: offset,
        count: count,
      });
    },
    // Send a direct signal to a creature/program entity. When `data` is a JSON
    // object without a correlationId, a correlationId is injected and the call
    // resolves to the VM's asynchronous result (key "creatures/signal/result")
    // instead of just the synchronous ACK.
    signal: async (
      creatureId: string,
      programId: string,
      entity: string,
      data: string,
      storeId?: string,
      temp?: boolean
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }

      let waitForResponse = false;
      let correlationId = "";
      let innerForSend = data;
      try {
        const parsed: any = JSONbig.parse(data);
        if (
          parsed &&
          typeof parsed === "object" &&
          !Array.isArray(parsed) &&
          !parsed.correlationId
        ) {
          correlationId = crypto.randomBytes(16).toString("hex");
          parsed.correlationId = correlationId;
          innerForSend = JSONbig.stringify(parsed);
          waitForResponse = true;
        }
      } catch {
        // data is not JSON — keep fire-and-forget behavior.
      }

      let responsePromise: Promise<{ resCode: number; obj: any }> | undefined;
      if (waitForResponse) {
        const timeoutMs = Number(
          process.env.CASPAR_SIGNAL_TIMEOUT_MS || "30000"
        );
        const cid = correlationId;
        responsePromise = new Promise<{ resCode: number; obj: any }>(
          (resolve) => {
            let timer: NodeJS.Timeout | undefined;
            this.pendingSignalResponses[cid] = (resp: any) => {
              if (timer) clearTimeout(timer);
              resolve({ resCode: 0, obj: resp });
            };
            timer = setTimeout(() => {
              if (this.pendingSignalResponses[cid]) {
                delete this.pendingSignalResponses[cid];
                resolve({
                  resCode: 32,
                  obj: {
                    message: `creature signal response timeout`,
                    correlationId: cid,
                    timeoutMs,
                  },
                });
              }
            }, timeoutMs);
          }
        );
      }

      const sendRes = await this.sendRequest(this.userId, "/creatures/signal", {
        type: "pvp",
        creatureId,
        // Forward programId/entityId at the top level so the node routes the
        // packet to the program's VM listener (registered under the programId).
        programId,
        entityId: entity,
        storeId,
        temp,
        data: JSONbig.stringify({ programId, entity, payload: innerForSend }),
      });
      if (!waitForResponse || sendRes.resCode !== 0) {
        if (
          waitForResponse &&
          correlationId &&
          this.pendingSignalResponses[correlationId]
        ) {
          delete this.pendingSignalResponses[correlationId];
        }
        return sendRes;
      }
      return responsePromise!;
    },
    create: async (payload: any): Promise<{ resCode: number; obj: any }> => {
      return await this.sendRequest("", "/creatures/create", payload);
    },
    delete: async (payload: any): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      return await this.sendRequest(this.userId, "/creatures/delete", payload);
    },
    update: async (payload: any): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      return await this.sendRequest(this.userId, "/creatures/update", payload);
    },
  };
  public programs = {
    // Create a machine-type creature (the identity a program is deployed under).
    createApp: async (
      chainId: string,
      username: string,
      title: string,
      desc: string
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      return await this.sendRequest(this.userId, "/creatures/create", {
        type: "machine",
        username: username,
        chainId: chainId,
        metadata: {
          public: {
            profile: {
              title: title,
              avatar: "123",
              desc: desc,
            },
          },
        },
      });
    },
    // Create a program under a creature.
    createMachine: async (
      username: string,
      appId: string,
      path: string,
      runtime: string,
      comment: string,
      publicKey: string
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      return await this.sendRequest(this.userId, "/programs/create", {
        username: username,
        appId: appId,
        path: path,
        publicKey: publicKey,
        runtime: runtime,
        comment: comment,
      });
    },
    deleteMachine: async (
      machineId: string
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      return await this.sendRequest(this.userId, "/programs/delete", {
        programId: machineId,
      });
    },
    updateMachine: async (
      machineId: string,
      path: string,
      metadata: any,
      promptFile?: string
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      if (promptFile) {
        metadata["prompt"] = fs.readFileSync(promptFile, { encoding: "utf-8" });
      }
      return await this.sendRequest(this.userId, "/programs/update", {
        programId: machineId,
        path: path,
        metadata: metadata,
      });
    },
    // Deploy a built entity to a program. `byteCode` is base64 of the primary
    // artifact (the payload); `runtime` becomes the entity type; `metadata.files`
    // may carry extra source files (base64) beside the payload.
    deploy: async (
      machineId: string,
      byteCode: string,
      runtime: string,
      metadata: { [key: string]: any }
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      if (runtime == "docker") {
        if (!metadata["imageName"] && !metadata["standalone"]) {
          return {
            resCode: 100,
            obj: { message: "docker image name must be specified" },
          };
        }
        if (!metadata["files"]) {
          return {
            resCode: 101,
            obj: { message: "source files must be specified" },
          };
        }
      }
      const entityId =
        (metadata && (metadata.entityId || metadata.entity)) || "main";
      const downloadable = !!(metadata && metadata.downloadable);
      return await this.sendRequest(this.userId, "/programs/deploy", {
        machineId: machineId,
        entityId: entityId,
        entityType: runtime,
        payload: byteCode,
        downloadable: downloadable,
        metadata: metadata,
      });
    },
    runMachine: async (
      machineId: string
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      return await this.sendRequest(this.userId, "/programs/runEntity", {
        machineId: machineId,
        entityId: "main",
      });
    },
    stopMachine: async (
      machineId: string
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      return await this.sendRequest(this.userId, "/programs/stopEntity", {
        programId: machineId,
        entityId: "main",
      });
    },
    // Permanently destroy one VM instance of a program entity. Unlike
    // stopMachine (which suspends and can be resumed), this removes the
    // instance and everything it owns — only the program's owner may call it.
    deleteVm: async (
      machineId: string,
      vmId: string,
      entityId: string = "main"
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      return await this.sendRequest(this.userId, "/programs/deleteEntity", {
        programId: machineId,
        entityId,
        vmId,
      });
    },
    listApps: async (
      offset: number,
      count: number
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      return await this.sendRequest(this.userId, "/creatures/list", {
        offset: offset,
        count: count,
      });
    },
    listMachines: async (
      offset: number,
      count: number
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      return await this.sendRequest(this.userId, "/programs/list", {
        offset: offset,
        count: count,
      });
    },
    readBuildLogs: async (
      vmId: string = "main",
      logType: string = "build",
      offset: number = 0,
      count: number = 200
    ): Promise<{ resCode: number; obj: any }> => {
      if (!this.userId) {
        return {
          resCode: USER_ID_NOT_SET_ERR_CODE,
          obj: { message: USER_ID_NOT_SET_ERR_MSG },
        };
      }
      // The node registers this under /machines (not /programs), and docker
      // BUILD output is recorded before any VM exists, under the runtime's
      // node-wide "main" stream with logType "build" — see the docker VM
      // plugin's emit_vm_log calls.
      return await this.sendRequest(this.userId, "/machines/readVmLogs", {
        vmId: vmId || "main",
        logType: logType || "build",
        offset,
        count,
      });
    },
  };
}

// ── VM project scaffolding ───────────────────────────────────────────────────
//
// A deployable Caspar VM project follows the convention `programs.deploy`
// expects:
//
//   <project>/
//   ├── builder/build.sh   # run from ./builder, produces ./bytecode (payload)
//   ├── src/<entity file>  # shipped alongside the payload as metadata.files
//   ├── vm.json            # local descriptor (runtime, entityId, deploy hints)
//   └── README.md
//
// `vm.init <runtime>` writes a minimal but valid, ready-to-deploy project for
// the requested runtime.

interface VmTemplate {
  entityFile: string;
  buildScript: string;
  files: { [name: string]: string };
  deployRuntime: string;
  deployNote: string;
}

function vmTemplate(runtime: VmRuntime, entityId: string): VmTemplate {
  switch (runtime) {
    case "wasm":
      return {
        entityFile: "module.wasm",
        deployRuntime: "wasm",
        deployNote:
          "Build your creature to src/module.wasm (build.sh compiles src/module.wat with wat2wasm when present).",
        buildScript: `#!/usr/bin/env bash
# Produce ./bytecode (the wasm payload) from the creature sources in ../src.
set -e
if command -v wat2wasm >/dev/null 2>&1 && [ -f ../src/module.wat ]; then
  wat2wasm ../src/module.wat -o ../src/module.wasm
fi
if [ ! -f ../src/module.wasm ]; then
  echo "src/module.wasm not found — build your creature module first." >&2
  exit 1
fi
cp ../src/module.wasm ./bytecode
`,
        files: {
          "module.wat": `;; Minimal Caspar wasm creature (placeholder).
;; A real creature imports the host ABI (hostCall) and exports \`update\`
;; plus \`malloc\`. See wiki/05-caspar-protocol.md and
;; wiki/07-vm-types-and-implementation.md.
(module
  (memory (export "memory") 1)
  (func (export "update"))
)
`,
        },
      };
    case "fire":
      return {
        entityFile: "module.wasm",
        deployRuntime: "fire",
        deployNote:
          "Firecracker microVM entity. Ship src/module.wasm (or rootfs/kernel refs in vm.json).",
        buildScript: `#!/usr/bin/env bash
# Produce ./bytecode from the microVM entity in ../src.
set -e
if command -v wat2wasm >/dev/null 2>&1 && [ -f ../src/module.wat ]; then
  wat2wasm ../src/module.wat -o ../src/module.wasm
fi
if [ ! -f ../src/module.wasm ]; then
  echo "src/module.wasm not found — build your entity first." >&2
  exit 1
fi
cp ../src/module.wasm ./bytecode
`,
        files: {
          "module.wat": `;; Minimal Firecracker entity (placeholder wasm payload).
(module
  (memory (export "memory") 1)
  (func (export "update"))
)
`,
        },
      };
    case "javascript":
      return {
        entityFile: "module.js",
        deployRuntime: "javascript",
        deployNote:
          "JavaScript entity — executed on the managed wasm runtime; can be transpiled to MASM for provable execution.",
        buildScript: `#!/usr/bin/env bash
# The JavaScript source IS the payload.
set -e
cp ../src/module.js ./bytecode
`,
        files: {
          "module.js": `// Caspar JavaScript creature entity.
// \`onSignal\` receives the JSON signal payload and returns the entity's output.
function onSignal(input) {
  const data = JSON.parse(input || "{}");
  return { ok: true, runtime: "javascript", echo: data };
}
globalThis.onSignal = onSignal;
`,
        },
      };
    case "docker":
      return {
        entityFile: "Dockerfile",
        deployRuntime: "docker",
        deployNote:
          'Docker entity. Deploy with metadata carrying imageName, e.g. \'{"imageName":"my-app:latest"}\'. Extra src files form the build context.',
        buildScript: `#!/usr/bin/env bash
# The Dockerfile IS the payload; the node builds the image from it on deploy.
set -e
cp ../src/Dockerfile ./bytecode
`,
        files: {
          Dockerfile: `# Caspar Docker entity — a long-lived HTTP server the node can proxy to.
FROM alpine:3.20
WORKDIR /app
COPY . /app
RUN apk add --no-cache python3
EXPOSE 8080
CMD ["python3", "-m", "http.server", "8080"]
`,
          "index.html": `<!doctype html><title>Caspar docker entity</title>
<h1>Hello from a Caspar docker VM</h1>
`,
        },
      };
    case "modal":
      return {
        entityFile: "Modalfile",
        deployRuntime: "modal",
        deployNote:
          'Modal cloud sandbox entity. The Modalfile is a Dockerfile-style recipe layered on a registry base image; the node needs MODAL_API_KEY configured.',
        buildScript: `#!/usr/bin/env bash
# The Modalfile IS the payload; the node resolves it into a Modal image.
set -e
cp ../src/Modalfile ./bytecode
`,
        files: {
          Modalfile: `# Caspar Modal entity — runs as a Modal cloud sandbox.
# The FROM line names the registry base image; everything after it is layered
# on top by Modal's builder.
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends python3 \\
  && rm -rf /var/lib/apt/lists/*
EXPOSE 8080
CMD ["python3", "-m", "http.server", "8080"]
`,
        },
      };
    case "elpian":
      return {
        entityFile: "module.elpian.json",
        deployRuntime: "elpian",
        deployNote:
          "Elpian AST entity — interpreted in-process. src/module.elpian.json holds the AST.",
        buildScript: `#!/usr/bin/env bash
# The Elpian AST JSON IS the payload.
set -e
cp ../src/module.elpian.json ./bytecode
`,
        files: {
          "module.elpian.json": `{
  "type": "program",
  "name": "hello",
  "body": [
    { "type": "return", "value": { "type": "literal", "value": "hello from elpian" } }
  ]
}
`,
        },
      };
    case "elpify":
      return {
        entityFile: "module.elpify.js",
        deployRuntime: "elpify",
        deployNote:
          "Elpify provable entity — JS transpiled to MASM and executed with a STARK proof (buildOnDeploy).",
        buildScript: `#!/usr/bin/env bash
# The Elpify JS source IS the payload; the node transpiles it to MASM on deploy.
set -e
cp ../src/module.elpify.js ./bytecode
`,
        files: {
          "module.elpify.js": `// Caspar Elpify provable entity.
// Transpiled to MASM and executed by the STARK-proving VM.
function main(a, b) {
  return a + b;
}
`,
        },
      };
  }
}

function scaffoldVmProject(
  runtime: string,
  targetPath: string,
  entityId: string
): { resCode: number; obj: any } {
  if (!(VM_RUNTIMES as readonly string[]).includes(runtime)) {
    return {
      resCode: 30,
      obj: {
        message: `unknown vm runtime '${runtime}'. Supported: ${VM_RUNTIMES.join(
          ", "
        )}`,
      },
    };
  }
  const abs = path.resolve(targetPath);
  if (fs.existsSync(abs) && fs.readdirSync(abs).length > 0) {
    return {
      resCode: 31,
      obj: { message: `target directory is not empty: ${abs}` },
    };
  }
  const eid = entityId || "main";
  const tpl = vmTemplate(runtime as VmRuntime, eid);

  fs.mkdirSync(path.join(abs, "src"), { recursive: true });
  fs.mkdirSync(path.join(abs, "builder"), { recursive: true });

  for (const [name, content] of Object.entries(tpl.files)) {
    fs.writeFileSync(path.join(abs, "src", name), content);
  }
  const buildPath = path.join(abs, "builder", "build.sh");
  fs.writeFileSync(buildPath, tpl.buildScript);
  try {
    fs.chmodSync(buildPath, 0o755);
  } catch {
    /* best-effort on platforms without chmod */
  }

  const deployMeta: { [k: string]: any } = { entityId: eid };
  if (runtime === "docker") deployMeta.imageName = "caspar-" + eid + ":latest";
  fs.writeFileSync(
    path.join(abs, "vm.json"),
    JSON.stringify(
      {
        runtime: tpl.deployRuntime,
        entityId: eid,
        entityFileName: tpl.entityFile,
        deploy: { runtime: tpl.deployRuntime, metadata: deployMeta },
      },
      null,
      2
    ) + "\n"
  );

  const metaStr = JSON.stringify(deployMeta);
  fs.writeFileSync(
    path.join(abs, "README.md"),
    `# Caspar ${runtime} VM project

${tpl.deployNote}

## Layout

- \`src/${tpl.entityFile}\` — the entity source (shipped with the deploy).
- \`builder/build.sh\` — produces \`builder/bytecode\` (the deploy payload).
- \`vm.json\` — local descriptor of the runtime + entity.

## Deploy to Caspar

\`\`\`bash
# 1. create the creature that owns the program (once):
caspar-client creatures.createMachine 1 my-${runtime}-app "My ${runtime} app" "demo"

# 2. create the program under that creature:
caspar-client programs.create ${runtime}entity <creatureId> /api/main ${runtime} "entry"

# 3. deploy this project to the program:
caspar-client programs.deploy <programId> ${targetPath} ${tpl.deployRuntime} '${metaStr}'

# 4. run it:
caspar-client programs.run <programId>
\`\`\`
`
  );

  return {
    resCode: 0,
    obj: {
      message: `scaffolded ${runtime} VM project`,
      path: abs,
      entityFile: tpl.entityFile,
      runtime: tpl.deployRuntime,
      next: `caspar-client programs.deploy <programId> ${targetPath} ${tpl.deployRuntime} '${metaStr}'`,
    },
  };
}

// ── Helpers ─────────────────────────────────────────────────────────────────

function isNumeric(str: string) {
  try {
    BigInt(str);
    return true;
  } catch {
    return false;
  }
}

async function executeBash(command: string) {
  return new Promise((resolve, reject) => {
    let dir = exec.exec(command, function (err, stdout, stderr) {
      if (err) {
        reject(err);
      }
      console.log(stdout);
    });
    dir.on("exit", function (code) {
      resolve(code);
    });
  });
}

const rl = readline.createInterface({
  input: process.stdin,
  output: process.stdout,
});

const envHost = process.env.CASPAR_HOST;
const envProto = process.env.CASPAR_PROTO || "ws";
const envPortStr = process.env.CASPAR_PORT;
const envPort = envPortStr ? parseInt(envPortStr, 10) : undefined;
let app = new Caspar(envProto, envHost, envPort);

// ── Command table ────────────────────────────────────────────────────────────

const commands: {
  [key: string]: (args: string[]) => Promise<{ resCode: number; obj: any }>;
} = {
  login: async (args: string[]): Promise<{ resCode: number; obj: any }> => {
    if (args.length < 1 || args.length > 2) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    return await app.login(args[0], args[1]);
  },
  logout: async (args: string[]): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 0) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    return app.logout();
  },
  printPrivateKey: async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 0) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    console.log("");
    console.log(app.myPrivateKey());
    console.log("");
    return { resCode: 0, obj: { message: "printed." } };
  },
  "creatures.me": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 0) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    return app.creatures.me();
  },
  "creatures.get": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 1) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    return app.creatures.get(args[0]);
  },
  "creatures.lockToken": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 3) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    if (!isNumeric(args[0])) {
      return {
        resCode: 30,
        obj: { message: "invalid numeric value: amount --> " + args[0] },
      };
    }
    return app.creatures.lockToken(Number(args[0]), args[1], args[2]);
  },
  "creatures.consumeLock": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 3) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    if (!isNumeric(args[2])) {
      return {
        resCode: 30,
        obj: { message: "invalid numeric value: amount --> " + args[2] },
      };
    }
    return app.creatures.consumeLock(args[0], args[1], Number(args[2]));
  },
  "creatures.list": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 2) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    if (!isNumeric(args[0])) {
      return {
        resCode: 30,
        obj: { message: "invalid numeric value: offset --> " + args[0] },
      };
    }
    if (!isNumeric(args[1])) {
      return {
        resCode: 30,
        obj: { message: "invalid numeric value: count --> " + args[1] },
      };
    }
    return app.creatures.list(Number(args[0]), Number(args[1]));
  },
  "creatures.signal": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 4 && args.length !== 5) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    return app.creatures.signal(
      args[0],
      args[1],
      args[2],
      args[3],
      args.length === 5 ? args[4] : undefined
    );
  },
  "creatures.createMachine": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 4) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    return await app.programs.createApp(args[0], args[1], args[2], args[3]);
  },
  "creatures.listMachines": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 2) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    if (!isNumeric(args[0])) {
      return {
        resCode: 30,
        obj: { message: "invalid numeric value: offset --> " + args[0] },
      };
    }
    if (!isNumeric(args[1])) {
      return {
        resCode: 30,
        obj: { message: "invalid numeric value: count --> " + args[1] },
      };
    }
    return await app.programs.listApps(Number(args[0]), Number(args[1]));
  },
  "programs.create": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 5) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    return await app.programs.createMachine(
      args[0],
      args[1],
      args[2],
      args[3],
      args[4],
      ""
    );
  },
  "programs.delete": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 1) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    return await app.programs.deleteMachine(args[0]);
  },
  "programs.update": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 3 && args.length !== 4) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    let metadata: any = {};
    try {
      metadata = JSONbig.parse(args[2]);
    } catch (ex) {
      try {
        metadata = JSON.parse(fs.readFileSync(args[2], { encoding: "utf-8" }));
      } catch (ex) {
        return { resCode: 30, obj: { message: "invalid metadata json" } };
      }
    }
    if (args.length == 4) {
      return await app.programs.updateMachine(
        args[0],
        args[1],
        metadata,
        args[3]
      );
    } else {
      return await app.programs.updateMachine(args[0], args[1], metadata);
    }
  },
  // Deploy a prebuilt artifact directly from a file path, skipping the
  // builder/src convention used by `programs.deploy`.
  "programs.deployRaw": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length < 3 || args.length > 5) {
      return {
        resCode: 30,
        obj: {
          message:
            "usage: programs.deployRaw [programId] [entityId] [artifactPath] [optional runtime=wasm] [optional metadataJson]",
        },
      };
    }
    const [machineId, entityId, artifactPath, runtimeArg, metaArg] = args;
    const runtime = runtimeArg || "wasm";
    let metadata: any = {};
    if (metaArg) {
      try {
        metadata = JSONbig.parse(metaArg);
      } catch (ex) {
        return { resCode: 30, obj: { message: "invalid metadata json" } };
      }
    }
    metadata.entityId = entityId;
    let bc: Buffer;
    try {
      bc = fs.readFileSync(artifactPath);
    } catch (ex: any) {
      return {
        resCode: 30,
        obj: { message: `cannot read artifact file: ${ex.message}` },
      };
    }
    return await app.programs.deploy(
      machineId,
      bc.toString("base64"),
      runtime,
      metadata
    );
  },
  // Deploy a VM project folder scaffolded by `vm.init`: runs builder/build.sh,
  // ships builder/bytecode as the payload and every src/ file as metadata.files.
  "programs.deploy": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 4) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    let metadata: any = {};
    try {
      metadata = JSONbig.parse(args[3]);
    } catch (ex) {
      return { resCode: 30, obj: { message: "invalid metadata json" } };
    }
    await executeBash(`cd ${args[1]}/builder && bash build.sh`);
    let bc = fs.readFileSync(`${args[1]}/builder/bytecode`);
    let files: { [name: string]: string } = {};
    fs.readdirSync(`${args[1]}/src`, { withFileTypes: true })
      .filter((item) => !item.isDirectory())
      .map((item) => {
        files[item.name] = fs
          .readFileSync(`${args[1]}/src/${item.name}`)
          .toString("base64");
      });
    metadata["files"] = files;
    return await app.programs.deploy(
      args[0],
      bc.toString("base64"),
      args[2],
      metadata
    );
  },
  "programs.run": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 1) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    return await app.programs.runMachine(args[0]);
  },
  "programs.stop": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 1) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    return await app.programs.stopMachine(args[0]);
  },
  "programs.list": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 2) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    if (!isNumeric(args[0])) {
      return {
        resCode: 30,
        obj: { message: "invalid numeric value: offset --> " + args[0] },
      };
    }
    if (!isNumeric(args[1])) {
      return {
        resCode: 30,
        obj: { message: "invalid numeric value: count --> " + args[1] },
      };
    }
    return await app.programs.listMachines(Number(args[0]), Number(args[1]));
  },
  "programs.readBuildLogs": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length > 2) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    return await app.programs.readBuildLogs(args[0] || "main", args[1] || "build");
  },
  // ── VM project templates (offline) ─────────────────────────────────────────
  "vm.types": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length !== 0) {
      return { resCode: 30, obj: { message: "invalid parameters count" } };
    }
    return {
      resCode: 0,
      obj: {
        runtimes: VM_RUNTIMES,
        note: "Use `vm.init <runtime> <path> [entityId]` to scaffold a deployable project.",
      },
    };
  },
  "vm.init": async (
    args: string[]
  ): Promise<{ resCode: number; obj: any }> => {
    if (args.length < 2 || args.length > 3) {
      return {
        resCode: 30,
        obj: { message: "usage: vm.init <runtime> <path> [entityId]" },
      };
    }
    return scaffoldVmProject(args[0], args[1], args[2] || "main");
  },
};

// ── Help ─────────────────────────────────────────────────────────────────────

const helpEntries: { [key: string]: string } = {
  login: `login [username] [optional email]
  → Authenticate against the node's /creatures/login and store credentials in ./auth.
  Example: login alice alice@example.com`,
  logout: `logout
  → Clear local auth state.
  Example: logout`,
  printPrivateKey: `printPrivateKey
  → Print your account private key body.
  Example: printPrivateKey`,
  "creatures.me": `creatures.me
  → Get the current creature profile.
  Example: creatures.me`,
  "creatures.get": `creatures.get [creatureId]
  → Get a creature by id.
  Example: creatures.get 123@global`,
  "creatures.list": `creatures.list [offset] [count]
  → List creatures.
  Example: creatures.list 0 10`,
  "creatures.lockToken": `creatures.lockToken [amount] [type] [target]
  → Lock tokens.
  Example: creatures.lockToken 100 pay 145@global`,
  "creatures.consumeLock": `creatures.consumeLock [lockId] [type] [amount]
  → Consume a token lock.
  Example: creatures.consumeLock 4f0f02a8d0 pay 100`,
  "creatures.signal": `creatures.signal [creatureId] [programId] [entity] [data] [optional storeId]
  → Send a direct signal to a creature/program entity and await its result.
  Example: creatures.signal 123@global 456@global main '{"cmd":"ping"}'`,
  "creatures.createMachine": `creatures.createMachine [chainId] [username] [title] [desc]
  → Create a machine-type creature (owns programs).
  Example: creatures.createMachine 1 calcapp Calculator "simple calc app"`,
  "creatures.listMachines": `creatures.listMachines [offset] [count]
  → List machine creatures.
  Example: creatures.listMachines 0 15`,
  "programs.create": `programs.create [username] [creatureId] [path] [runtime] [comment]
  → Create a program under a creature.
  Example: programs.create calculator 984@global /api/sum wasm "sum machine"`,
  "programs.delete": `programs.delete [programId]
  → Delete a program.
  Example: programs.delete 876@global`,
  "programs.update": `programs.update [programId] [path] [metadataJsonOrFilePath] [optional promptFile]
  → Update a program's path/metadata.
  Example: programs.update 876@global /api/sum '{"public":{"profile":{"title":"Calc"}}}'`,
  "programs.deploy": `programs.deploy [programId] [projectFolderPath] [runtime] [metadata]
  → Build (builder/build.sh) and deploy a VM project folder (see vm.init).
  Example: programs.deploy 876@global ./calc-proj wasm '{}'`,
  "programs.deployRaw": `programs.deployRaw [programId] [entityId] [artifactPath] [optional runtime=wasm] [optional metadataJson]
  → Deploy a prebuilt artifact file directly (no builder/src convention).
  Example: programs.deployRaw 876@global main ./module.wasm wasm '{}'`,
  "programs.run": `programs.run [programId]
  → Run the program's main entity.
  Example: programs.run 876@global`,
  "programs.stop": `programs.stop [programId]
  → Stop the program's main entity.
  Example: programs.stop 876@global`,
  "programs.list": `programs.list [offset] [count]
  → List programs.
  Example: programs.list 0 15`,
  "programs.readBuildLogs": `programs.readBuildLogs [optional vmId] [optional logType]
  → Read VM logs. Docker image BUILD output is recorded under the runtime's
  node-wide "main" stream with logType "build" — the defaults — so a bare
  \`programs.readBuildLogs\` shows the current image build. Pass a real vmId
  with logType "runtime" for a running VM's logs.
  Example1: programs.readBuildLogs
  Example2: programs.readBuildLogs 3bb9ed93-b842-4f8d-af36-6251969c62d6 runtime`,
  "vm.types": `vm.types
  → List the six VM runtimes a Caspar node supports.
  Example: vm.types`,
  "vm.init": `vm.init [runtime] [path] [optional entityId]
  → Scaffold a deployable VM project for one of: ${VM_RUNTIMES.join(", ")}.
  Example: vm.init wasm ./my-wasm-vm main`,
};

const fullHelp = `Caspar Client CLI – Command Reference
Every command maps directly to a Caspar node shell action route.

${Object.values(helpEntries).join("\n\n")}

help [optional command]
  → Show full help or command-specific help.
  Example1: help
  Example2: help creatures.signal

Connection (env vars):
  CASPAR_HOST   node host (default 127.0.0.1)
  CASPAR_PROTO  ws | tcp (default ws)
  CASPAR_PORT   action port (ws default 8076, tcp default 8077)
  CASPAR_INSECURE=1  skip TLS certificate verification (dev only)

Non-interactive mode:
  1) Single command: caspar-client <command> [args...]
     Example: caspar-client creatures.me
  2) Batch inline:   caspar-client --batch "creatures.me; programs.list 0 10"
  3) Batch file:     caspar-client --batch-file ./commands.txt
`;

// ── Runner ───────────────────────────────────────────────────────────────────

function parseCommandParts(str: string): string[] {
  let parts: string[] = [];
  let inVal = false;
  let valEdge = "";
  let temp = "";
  for (let i = 0; i < str.length; i++) {
    if (inVal) {
      if (str[i] === valEdge) {
        inVal = false;
        valEdge = "";
      } else {
        temp += str[i];
      }
    } else {
      if (str[i] === "'" || str[i] === '"') {
        inVal = true;
        valEdge = str[i];
      } else if (str[i] === " ") {
        if (temp !== "") {
          parts.push(temp);
          temp = "";
        }
      } else {
        temp += str[i];
      }
    }
  }
  if (temp !== "") {
    parts.push(temp);
  }
  return parts;
}

async function runParsedCommand(parts: string[]): Promise<number> {
  if (parts.length === 0) return 0;
  if (parts.length === 1 && parts[0] === "help") {
    console.log(fullHelp);
    return 0;
  }
  if (parts.length === 2 && parts[0] === "help") {
    let itemHelp = helpEntries[parts[1]];
    if (itemHelp) {
      console.log(itemHelp + "\n");
      return 0;
    }
    console.log(`help not found for command: ${parts[1]}`);
    return 1;
  }
  if (parts.length === 1 && parts[0] === "clear") {
    console.clear();
    return 0;
  }
  let fn = commands[parts[0]];
  if (fn !== undefined) {
    let res = await fn(parts.slice(1));
    if (res.resCode == 0) {
      console.log(res.obj);
      return 0;
    }
    console.log("Error: ", res.obj);
    return res.resCode;
  }
  console.log("command not detected.");
  return 1;
}

// Offline commands never touch the node, so they don't require authentication.
const OFFLINE_COMMANDS = new Set([
  "login",
  "help",
  "clear",
  "vm.types",
  "vm.init",
]);
function commandRequiresAuth(command: string): boolean {
  return !OFFLINE_COMMANDS.has(command);
}

async function runNonInteractive(argv: string[]): Promise<number> {
  const firstArg = argv[0]?.trim();
  if (firstArg === "--help" || firstArg === "-h") {
    return await runParsedCommand(["help", ...argv.slice(1)]);
  }
  // Offline-only fast paths: browse help / scaffold templates without a node.
  if (
    firstArg === "help" ||
    firstArg === "clear" ||
    firstArg === "vm.types" ||
    firstArg === "vm.init"
  ) {
    return await runParsedCommand(parseCommandParts(argv.join(" ").trim()));
  }
  await app.connectTransport();

  let batches: string[] = [];
  if (argv[0] === "--batch") {
    const raw = argv.slice(1).join(" ").trim();
    batches = raw
      .split(";")
      .map((x) => x.trim())
      .filter((x) => x.length > 0);
  } else if (argv[0] === "--batch-file" && argv[1]) {
    const raw = fs.readFileSync(argv[1], { encoding: "utf-8" });
    batches = raw
      .split("\n")
      .map((x) => x.trim())
      .filter((x) => x.length > 0 && !x.startsWith("#"));
  } else {
    batches = [argv.join(" ").trim()];
  }

  for (let i = 0; i < batches.length; i++) {
    const parts = parseCommandParts(batches[i]);
    if (parts.length === 0) continue;

    if (commandRequiresAuth(parts[0])) {
      if (!app.hasCredentials()) {
        console.log(
          "Error: not authenticated. Please login first using: login [username]"
        );
        return 10;
      }
      const auth = await app.authenticateSession();
      if (auth.resCode !== 0) {
        console.log(
          "Error: authentication failed. Please login again with: login [username]"
        );
        console.log(auth.obj);
        return auth.resCode;
      }
    }

    const code = await runParsedCommand(parts);
    if (code !== 0) return code;
  }
  return 0;
}

let ask = () => {
  rl.question(`${app.myUsername()}$ `, async (q) => {
    let str = q.trim();
    let parts = parseCommandParts(str);
    await runParsedCommand(parts);
    setTimeout(() => {
      ask();
    });
  });
};

(async () => {
  console.clear();
  const argv = process.argv.slice(2);
  if (argv.length > 0) {
    const code = await runNonInteractive(argv);
    process.exit(code);
  }
  await app.connect();
  console.log(
    'Welcome to the Caspar client shell — enter a command or "help" for the command reference:\n'
  );
  ask();
})();
