package module_core

import (
	"crypto/rsa"
	"crypto/sha256"
	"crypto/x509"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"encoding/pem"
	"kasper/src/abstract/adapters/docker"
	"kasper/src/abstract/adapters/elpis"
	"kasper/src/abstract/adapters/file"
	"kasper/src/abstract/adapters/firectl"
	"kasper/src/abstract/adapters/network"
	"kasper/src/abstract/adapters/security"
	"kasper/src/abstract/adapters/signaler"
	"kasper/src/abstract/adapters/storage"
	"kasper/src/abstract/adapters/tools"
	"kasper/src/abstract/adapters/wasm"
	iaction "kasper/src/abstract/models/action"
	"kasper/src/abstract/models/chain"
	"kasper/src/abstract/models/info"
	"kasper/src/abstract/models/input"
	"kasper/src/abstract/models/trx"
	"kasper/src/abstract/models/update"
	"kasper/src/abstract/models/worker"
	"kasper/src/abstract/state"
	actor "kasper/src/core/module/actor"
	mainstate "kasper/src/core/module/actor/model/state"
	module_trx "kasper/src/core/module/actor/model/trx"
	inputs_users "kasper/src/shell/api/inputs/users"
	mach_model "kasper/src/shell/api/model"
	"kasper/src/shell/utils/crypto"
	"kasper/src/shell/utils/future"

	driver_file "kasper/src/drivers/file"
	driver_firectl "kasper/src/drivers/firectl"
	driver_network "kasper/src/drivers/network"
	driver_security "kasper/src/drivers/security"
	driver_signaler "kasper/src/drivers/signaler"
	driver_storage "kasper/src/drivers/storage"
	driver_vmm "kasper/src/drivers/vmm"

	driver_network_fed "kasper/src/drivers/network/federation"

	"log"
	"os"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	cryp "crypto"
	"crypto/rand"
)

type Tools struct {
	security security.ISecurity
	signaler signaler.ISignaler
	storage  storage.IStorage
	network  network.INetwork
	file     file.IFile
	vmm      wasm.IWasm
	elpis    elpis.IElpis
	docker   docker.IDocker
	firectl  firectl.IFirectl
}

func (t *Tools) Security() security.ISecurity {
	return t.security
}

func (t *Tools) Signaler() signaler.ISignaler {
	return t.signaler
}

func (t *Tools) Storage() storage.IStorage {
	return t.storage
}

func (t *Tools) Network() network.INetwork {
	return t.network
}

func (t *Tools) File() file.IFile {
	return t.file
}

func (t *Tools) Vmm() wasm.IWasm {
	return t.vmm
}

func (t *Tools) Elpis() elpis.IElpis {
	return t.elpis
}

func (t *Tools) Docker() docker.IDocker {
	return t.docker
}

func (t *Tools) Firectl() firectl.IFirectl {
	return t.firectl
}

type Core struct {
	lock                   sync.Mutex
	triggerLock            sync.Mutex
	ownerId                string
	ownerPrivKey           *rsa.PrivateKey
	id                     string
	tools                  tools.ITools
	started                bool
	gods                   []string
	chain                  chan any
	chainCallbacks         map[string]*chain.ChainCallback
	Ip                     string
	elections              []chain.Election
	elecReg                bool
	elecStarter            string
	elecStartTime          int64
	freeNodes              map[string]bool
	appPendingTrxs         []*worker.Trx
	actionStore            iaction.IActor
	privKey                *rsa.PrivateKey
	messageCallbacks       map[string]*chain.MessageCallback
	executionCostPerSecond int64
}

var MAX_VALIDATOR_COUNT = 5

type chainSubmission struct {
	chainId string
	op      any
}

func NewCore(origin string, ownerId string, ownerPrivateKey *rsa.PrivateKey) *Core {
	id := origin
	freeNodes := map[string]bool{}
	freeNodes[os.Getenv("ROOT_NODE")] = true
	return &Core{
		ownerId:                ownerId,
		ownerPrivKey:           ownerPrivateKey,
		id:                     id,
		gods:                   make([]string, 0),
		chain:                  nil,
		chainCallbacks:         map[string]*chain.ChainCallback{},
		messageCallbacks:       map[string]*chain.MessageCallback{},
		Ip:                     id,
		elections:              nil,
		elecReg:                false,
		freeNodes:              freeNodes,
		actionStore:            actor.NewActor(),
		started:                false,
		executionCostPerSecond: 0,
	}
}

func (c *Core) FreeNodes() map[string]bool {
	return c.freeNodes
}

func (c *Core) AddFreeNode(nodeId string) {
	if nodeId == "" {
		return
	}
	c.lock.Lock()
	defer c.lock.Unlock()
	if c.freeNodes == nil {
		c.freeNodes = map[string]bool{}
	}
	c.freeNodes[nodeId] = true
}

func (c *Core) Actor() iaction.IActor {
	return c.actionStore
}

func (c *Core) ModifyStateSecurlyWithSource(readonly bool, info info.IInfo, src string, fn func(state.IState) error) {
	trx := module_trx.NewTrx(c, c.Tools().Storage(), readonly)
	var err error
	defer func() {
		if err == nil {
			trx.Commit()
		} else {
			trx.Discard()
		}
	}()
	s := mainstate.NewState(info, trx, src)
	err = fn(s)
}

func (c *Core) ModifyStateSecurly(readonly bool, info info.IInfo, fn func(state.IState) error) {
	trx := module_trx.NewTrx(c, c.Tools().Storage(), readonly)
	var err error
	defer func() {
		if err == nil {
			trx.Commit()
		} else {
			trx.Discard()
		}
	}()
	s := mainstate.NewState(info, trx)
	err = fn(s)
}

func (c *Core) ModifyState(readonly bool, fn func(trx.ITrx) error) {
	trx := module_trx.NewTrx(c, c.Tools().Storage(), readonly)
	var err error
	defer func() {
		if err == nil {
			trx.Commit()
		} else {
			trx.Discard()
		}
	}()
	err = fn(trx)
}

func (c *Core) Tools() tools.ITools {
	return c.tools
}

func (c *Core) Id() string {
	return c.id
}

func (c *Core) OwnerId() string {
	return c.ownerId
}

func (c *Core) Gods() []string {
	return c.gods
}

func (c *Core) AddGod(username string) {
	if username == "" {
		return
	}
	c.lock.Lock()
	defer c.lock.Unlock()
	if slices.Contains(c.gods, username) {
		return
	}
	c.gods = append(c.gods, username)
}

func (c *Core) IpAddr() string {
	return c.Ip
}

func (c *Core) AppPendingTrxs() {
	elpisTrxs := []*worker.Trx{}
	wasmTrxs := []*worker.Trx{}
	for _, trx := range c.appPendingTrxs {
		if trx.Runtime == "elpis" {
			elpisTrxs = append(elpisTrxs, trx)
		} else if trx.Runtime == "wasm" {
			wasmTrxs = append(wasmTrxs, trx)
		}
	}
	if len(elpisTrxs) > 0 {
		c.Tools().Elpis().ExecuteChainTrxsGroup(elpisTrxs)
	}
	if len(wasmTrxs) > 0 {
		c.Tools().Vmm().ExecuteChainTrxsGroup(wasmTrxs)
	}
	c.appPendingTrxs = []*worker.Trx{}
}

func (c *Core) ClearAppPendingTrxs() {
	c.appPendingTrxs = []*worker.Trx{}
}

func (c *Core) SignPacket(data []byte) string {
	hashed := sha256.Sum256(data)
	signature, err := rsa.SignPSS(rand.Reader, c.privKey, cryp.SHA256, hashed[:], &rsa.PSSOptions{
		SaltLength: rsa.PSSSaltLengthEqualsHash,
	})
	if err != nil {
		panic(err)
	}
	return base64.StdEncoding.EncodeToString(signature)
}

func (c *Core) SignPacketAsOwner(data []byte) string {
	hashed := sha256.Sum256(data)
	signature, err := rsa.SignPSS(rand.Reader, c.ownerPrivKey, cryp.SHA256, hashed[:], &rsa.PSSOptions{
		SaltLength: rsa.PSSSaltLengthEqualsHash,
	})
	if err != nil {
		panic(err)
	}
	return base64.StdEncoding.EncodeToString(signature)
}

func (c *Core) PlantChainTrigger(count int, userId string, tag string, machineId string, pointId string, attachment string) {
	c.triggerLock.Lock()
	defer c.triggerLock.Unlock()
	c.ModifyState(false, func(trx trx.ITrx) error {
		tail := crypto.SecureUniqueString()
		found := (len(trx.GetByPrefix("chainCallback::"+userId+"_"+tag+"|>")) > 0)
		trx.PutBytes("chainCallback::"+userId+"_"+tag+"|>"+tail, []byte{0x01})
		trx.PutBytes("chainCallback::"+userId+"_"+tag+"|"+tail+"::machineId", []byte(machineId))
		trx.PutBytes("chainCallback::"+userId+"_"+tag+"|"+tail+"::pointId", []byte(pointId))
		trx.PutBytes("chainCallback::"+userId+"_"+tag+"|"+tail+"::attachment", []byte(attachment))
		if !found {
			targetCountB := make([]byte, 4)
			binary.BigEndian.PutUint32(targetCountB, uint32(count))
			trx.PutBytes("chainCallback::"+userId+"_"+tag+"::targetCount", targetCountB)
			tempCountB := make([]byte, 4)
			binary.BigEndian.PutUint32(tempCountB, uint32(0))
			trx.PutBytes("chainCallback::"+userId+"_"+tag+"::tempCount", tempCountB)
		}
		return nil
	})
}

func (c *Core) ExecBaseRequestOnChain(key string, payload []byte, signature string, userId string, tag string, callback func([]byte, int, error)) {
	c.lock.Lock()
	defer c.lock.Unlock()
	callbackId := crypto.SecureUniqueString()
	c.chainCallbacks[callbackId] = &chain.ChainCallback{Tag: tag, Fn: callback, Executors: map[string]bool{}, Responses: map[string]string{}}
	future.Async(func() {
		c.chain <- chain.ChainBaseRequest{Tag: tag, Signatures: []string{c.SignPacket(payload), signature}, Submitter: c.id, RequestId: callbackId, Author: "user::" + userId, Key: key, Payload: payload}
	}, false)
}

func (c *Core) SendMessageOnChain(key string, payload []byte, signature string, userId string, receivers map[string]map[string]bool, replyTo string, callback func(string, []byte)) {
	c.SendTypedMessageOnChain("main", key, "vm.execute", payload, signature, userId, receivers, replyTo, "", nil, callback)
}

func (c *Core) SendTypedMessageOnChain(chainId string, key string, messageType string, payload []byte, signature string, userId string, receivers map[string]map[string]bool, replyTo string, pointId string, pay *chain.ChainPayPacket, callback func(string, []byte)) {
	c.lock.Lock()
	defer c.lock.Unlock()
	callbackId := crypto.SecureUniqueString()
	if callback != nil {
		c.messageCallbacks[callbackId] = &chain.MessageCallback{Id: callbackId, Fn: callback}
	}
	if messageType == "" {
		messageType = "vm.execute"
	}
	if chainId == "" {
		chainId = "main"
	}
	future.Async(func() {
		c.chain <- chainSubmission{chainId: chainId, op: chain.ChainMessage{Key: key, MessageType: messageType, ReplyTo: replyTo, PointId: pointId, Pay: pay, Recievers: receivers, Signatures: []string{c.SignPacket(payload), signature}, Submitter: c.id, RequestId: callbackId, Author: "user::" + userId, Payload: payload}}
	}, false)
}

func (c *Core) chainMessageTargetsLocalNode(packet chain.ChainMessage) bool {
	if _, exists := packet.Recievers["*"]; exists {
		return true
	}
	_, exists := packet.Recievers[c.id]
	return exists
}

func (c *Core) chainMessageMachineIds(packet chain.ChainMessage) map[string]bool {
	machineIds := map[string]bool{}
	if ids, ok := packet.Recievers[c.id]; ok {
		for machineId := range ids {
			machineIds[machineId] = true
		}
	}
	if packet.Pay != nil {
		for _, machineId := range packet.Pay.MachineIds {
			machineIds[machineId] = true
		}
	}
	return machineIds
}

func (c *Core) chainSubmitMachineId(op any) string {
	switch t := op.(type) {
	case chain.ChainMessage:
		for machineId := range c.chainMessageMachineIds(t) {
			if machineId != "" {
				return machineId
			}
		}
	}
	return ""
}

func (c *Core) runChainMessage(packet chain.ChainMessage) {
	for machineId := range c.chainMessageMachineIds(packet) {
		var runtimeType string
		c.ModifyState(true, func(trx trx.ITrx) error {
			vm := mach_model.Vm{MachineId: machineId}.Pull(trx)
			runtimeType = vm.Runtime
			return nil
		})
		if runtimeType == "wasm" || runtimeType == "docker" {
			listener, found := c.Tools().Signaler().Listeners().Get(machineId)
			if found && listener != nil {
				payload := append([]byte(nil), packet.Payload...)
				future.Async(func() {
					listener.Signal("points/signal", payload)
				}, false)
				continue
			}
		}
		future.Async(func() {
			if runtimeType == "wasm" {
				c.Tools().Vmm().RunVm(machineId, packet.PointId, string(packet.Payload))
			}
		}, false)
	}
}

func (c *Core) consumePayLockOnChain(pay *chain.ChainPayPacket) bool {
	if pay == nil || pay.LockId == "" || pay.UserId == "" || pay.LockSignature == "" || pay.Amount <= 0 {
		return false
	}
	inp, _ := json.Marshal(inputs_users.ConsumeLockInput{
		Type:      "pay",
		UserId:    pay.UserId,
		LockId:    pay.LockId,
		Signature: pay.LockSignature,
		Amount:    pay.Amount,
	})
	sign := c.SignPacketAsOwner(inp)
	res := make(chan bool, 1)
	c.ExecBaseRequestOnChain("/users/consumeLock", inp, sign, c.ownerId, "", func(b []byte, i int, err error) {
		if err != nil || i >= 400 {
			res <- false
			return
		}
		res <- true
	})
	return <-res
}

func (c *Core) ExecBaseResponseOnChain(callbackId string, packet []byte, signature string, resCode int, e string, updates []update.Update, tag string, toUserId string) {
	future.Async(func() {
		sort.Slice(updates, func(i, j int) bool {
			return (updates[i].Typ + ":" + updates[i].Key) < (updates[j].Typ + ":" + updates[j].Key)
		})
		c.chain <- chain.ChainResponse{ToUserId: toUserId, Tag: tag, Signature: signature, Executor: c.id, RequestId: callbackId, ResCode: resCode, Err: e, Payload: packet, Effects: chain.Effects{DbUpdates: updates}}
	}, false)
}

func (c *Core) OnChainPacket(typ string, trxPayload []byte) string {
	c.lock.Lock()
	defer c.lock.Unlock()
	switch typ {
	case "message":
		{
			packet := chain.ChainMessage{}
			err := json.Unmarshal(trxPayload, &packet)
			if err != nil {
				log.Println(err)
				return ""
			}
			if packet.MessageType == "" {
				packet.MessageType = "vm.execute"
			}
			if packet.ReplyTo != "" {
				cb, ok := c.messageCallbacks[packet.ReplyTo]
				if !ok {
					return ""
				}
				cb.Fn(packet.Key, packet.Payload)
			} else {
				if !c.chainMessageTargetsLocalNode(packet) {
					return ""
				}
				switch packet.MessageType {
				case "vm.cost.negotiate":
					if packet.Author == c.id {
						return ""
					}
					pay := &chain.ChainPayPacket{Type: "vm.cost.ack", SessionId: packet.RequestId, CostPerSecond: c.executionCostPerSecond}
					future.Async(func() {
						c.chain <- chain.ChainMessage{Key: packet.Key, MessageType: "vm.cost.ack", ReplyTo: packet.RequestId, Recievers: map[string]map[string]bool{packet.Submitter: map[string]bool{}}, Signatures: []string{c.SignPacket([]byte(strconv.FormatInt(c.executionCostPerSecond, 10)))}, Submitter: c.id, RequestId: crypto.SecureUniqueString(), Author: c.id, Pay: pay}
					}, false)
				case "vm.execute.request", "vm.execute.charge", "vm.execute":
					if packet.Pay != nil && (packet.MessageType == "vm.execute.request" || packet.MessageType == "vm.execute.charge") {
						if !c.freeNodes[packet.Submitter] && !c.consumePayLockOnChain(packet.Pay) {
							return ""
						}
						packetCpy := packet
						if packetCpy.Pay.AcceptedSeconds <= 0 && c.executionCostPerSecond > 0 {
							packetCpy.Pay.AcceptedSeconds = packetCpy.Pay.Amount / c.executionCostPerSecond
						}
						future.Async(func() {
							c.runChainMessage(packetCpy)
						}, false)
						return ""
					}
					c.runChainMessage(packet)
				default:
					return ""
				}
			}
			break
		}
	case "base":
		{
			packet := chain.ChainBaseRequest{}
			err := json.Unmarshal(trxPayload, &packet)
			if err != nil {
				log.Println(err)
				return ""
			}
			execs := map[string]bool{}
			if packet.Submitter == c.id {
				c.chainCallbacks[packet.RequestId].Executors = execs
			} else {
				c.chainCallbacks[packet.RequestId] = &chain.ChainCallback{Fn: nil, Executors: execs, Responses: map[string]string{}}
			}
			userId := ""
			if strings.HasPrefix(packet.Author, "user::") {
				userId = packet.Author[len("user::"):]
			}
			action := c.actionStore.FetchAction(packet.Key)
			if action == nil {
				return ""
			}
			var input input.IInput
			i, err2 := action.(iaction.ISecureAction).ParseInput("chain", packet.Payload)
			if err2 != nil {
				log.Println(err2)
				errText := "input parsing error"
				signature := c.SignPacket([]byte(errText))
				c.ExecBaseResponseOnChain(packet.RequestId, []byte{}, signature, 400, errText, []update.Update{}, packet.Tag, userId)
				return ""
			}
			input = i
			resCode, res, err := action.(iaction.ISecureAction).SecurlyActChain(userId, packet.RequestId, packet.Payload, packet.Signatures[1], input, packet.Submitter, packet.Tag)
			if packet.Author == c.id {
				callback := c.chainCallbacks[packet.RequestId]
				delete(c.chainCallbacks, packet.RequestId)
				if callback.Fn != nil {
					if err == nil {
						resData, err := json.Marshal(res)
						if err != nil {
							log.Println(err)
							callback.Fn([]byte("{}"), resCode, err)
						} else {
							callback.Fn(resData, resCode, nil)
						}
					} else {
						callback.Fn([]byte("{}"), resCode, err)
					}
				}
			}
			break
		}
	}
	return ""
}

func (c *Core) Close() {
	c.tools.Network().Chain().Close()
	c.tools.Storage().KvDb().Close()
	c.tools.Storage().TsDb().Close()
	c.tools.Vmm().CloseKVDB()
}

func (c *Core) MarkAsStarted() {
	c.started = true
}

func (c *Core) Load(gods []string, args map[string]interface{}) {
	c.gods = gods

	sroot := args["storageRoot"].(string)
	bdbPath := args["baseDbPath"].(string)
	adbPath := args["appletDbPath"].(string)
	ldbPath := args["pointLogsDb"].(string)
	srchPath := args["searcherDb"].(string)

	dnFederation := driver_network_fed.FirstStageBackFill(c)
	dstorage := driver_storage.NewStorage(c, sroot, bdbPath, ldbPath, srchPath)
	dsignaler := driver_signaler.NewSignaler(c, dnFederation)
	dsecurity := driver_security.New(c, sroot, dstorage, dsignaler)
	dNetwork := driver_network.NewNetwork(c, dstorage, dsecurity, dsignaler, dnFederation)
	dFile := driver_file.NewFileTool(sroot)
	var dDocker docker.IDocker
	dVmm := driver_vmm.NewVmm(c, sroot, dstorage, adbPath, dDocker, dFile)
	var dElpis elpis.IElpis
	dnFederation.SecondStageForFill(dstorage, dFile, dsignaler)
	dFirectl := driver_firectl.NewFireCtl()

	pemData := dsecurity.FetchKeyPair("server_key")[0]
	block, _ := pem.Decode([]byte(pemData))
	if block == nil || block.Type != "PRIVATE KEY" {
		panic("failed to decode PEM block containing private key")
	}
	privateKey, err := x509.ParsePKCS8PrivateKey(block.Bytes)
	if err != nil {
		panic(err)
	}
	c.privKey = privateKey.(*rsa.PrivateKey)

	c.tools = &Tools{
		signaler: dsignaler,
		storage:  dstorage,
		security: dsecurity,
		network:  dNetwork,
		file:     dFile,
		docker:   dDocker,
		firectl:  dFirectl,
		vmm:      dVmm,
		elpis:    dElpis,
	}

	if cpsRaw := os.Getenv("VM_EXEC_COST_PER_SECOND"); cpsRaw != "" {
		if cps, err := strconv.ParseInt(cpsRaw, 10, 64); err == nil && cps >= 0 {
			c.executionCostPerSecond = cps
		}
	}

	c.tools.Network().Chain().RegisterPipeline(func(b [][]byte, insiderCb func([]byte)) []string {
		machineIds := []string{}
		for _, trx := range b {
			firstIndex := strings.Index(string(trx), "::")
			log.Println(string(trx))
			typ := string(trx[:firstIndex])
			if typ == "nodeJoined" {
				insiderCb(trx)
			} else if typ == ("sharderMap|" + c.id) {
				insiderCb(trx)
			} else {
				r := c.OnChainPacket(typ, trx[firstIndex+2:])
				if r != "" {
					machineIds = append(machineIds, r)
				}
			}
		}
		c.AppPendingTrxs()
		return machineIds
	})

	c.chain = make(chan any, 1)
	future.Async(func() {
		for {
			rawOp := <-c.chain
			typ := ""
			chainId := "main"
			op := rawOp
			if envelope, ok := rawOp.(chainSubmission); ok {
				op = envelope.op
				if envelope.chainId != "" {
					chainId = envelope.chainId
				}
			}
			switch op.(type) {
			case chain.ChainBaseRequest:
				{
					typ = "base"
					break
				}
			case chain.ChainMessage:
				{
					typ = "message"
					break
				}
			}
			if typ != "" {
				serialized, err := json.Marshal(op)
				if err == nil {
					log.Println(string(serialized))
					machineId := c.chainSubmitMachineId(op)
					c.tools.Network().Chain().SubmitTrx(chainId, machineId, typ, []byte(typ+"::"+string(serialized)))
				} else {
					log.Println(err)
				}
			}
		}
	}, true)

	future.Async(func() {
		for {
			time.Sleep(time.Duration(1) * time.Second)
			func() {
				defer func() {
					if err := recover(); err != nil {
						log.Println(err)
					}
				}()
				minutes := time.Now().Minute()
				seconds := time.Now().Second()
				if (minutes == 0) && ((seconds >= 0) && (seconds <= 2)) {
					c.DoElection()
					time.Sleep(2 * time.Minute)
				}
			}()
		}
	}, false)
}

func (c *Core) DoElection() {
	c.lock.Lock()
	defer c.lock.Unlock()
	c.elecReg = true
	future.Async(func() {
		c.chain <- chain.ChainElectionPacket{
			Type:    "election",
			Key:     "choose-validator",
			Meta:    map[string]any{"phase": "start-reg", "voter": c.Ip},
			Payload: []byte("{}"),
		}
	}, false)
}
