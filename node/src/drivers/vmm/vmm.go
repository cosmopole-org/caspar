package vmm

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"kasper/src/abstract/adapters/docker"
	"kasper/src/abstract/adapters/file"
	"kasper/src/abstract/adapters/signaler"
	"kasper/src/abstract/adapters/storage"
	"kasper/src/abstract/models/chain"
	"kasper/src/abstract/models/core"
	"kasper/src/abstract/models/trx"
	"kasper/src/abstract/models/worker"
	"kasper/src/abstract/state"
	"kasper/src/core/module/actor/model/base"
	inputs_points "kasper/src/shell/api/inputs/points"
	inputs_users "kasper/src/shell/api/inputs/users"
	"kasper/src/shell/api/model"
	updates_points "kasper/src/shell/api/updates/points"
	"kasper/src/shell/utils/future"
	"log"
	"net/http"
	"os"
	"strings"
	"time"

	zmq "github.com/pebbe/zmq4"
)

type Vmm struct {
	app         core.ICore
	storageRoot string
	storage     storage.IStorage
	docker      docker.IDocker
	file        file.IFile
	aeSocket    chan string
}

func (wm *Vmm) Assign(machineId string) {
	wm.app.Tools().Signaler().ListenToSingle(&signaler.Listener{
		Id: machineId,
		Signal: func(key string, a any) {
			astPath := wm.app.Tools().Storage().StorageRoot() + "/machines/" + machineId + "/module"
			vmType := "wasm"
			wm.app.ModifyState(true, func(trx trx.ITrx) error {
				vm := model.Vm{MachineId: machineId}.Pull(trx)
				if vm.Path != "" {
					astPath = vm.Path
				}
				if vm.Runtime != "" {
					vmType = vm.Runtime
				}
				return nil
			})
			data := string(a.([]byte))
			if key == "points/signal" {
				str, _ := json.Marshal(map[string]any{
					"type":      "runVm",
					"machineId": machineId,
					"input":     data,
					"astPath":   astPath,
					"vmType":    vmType,
				})
				wm.aeSocket <- string(str)
			}
		},
	})
}

func (wm *Vmm) ExecuteChainTrxsGroup(trxs []*worker.Trx) {
	_ = trxs
}

func (wm *Vmm) ExecuteChainEffects(effects string) {
	_ = effects
}

type ChainDbOp struct {
	OpType string `json:"opType"`
	Key    string `json:"key"`
	Val    string `json:"val"`
}

func (wm *Vmm) RunVm(machineId string, pointId string, data string) {
	point := model.Point{Id: pointId}
	isMemberOfPoint := false
	wm.app.ModifyState(true, func(trx trx.ITrx) error {
		point.Pull(trx)
		isMemberOfPoint = (trx.GetLink("memberof::"+machineId+"::"+pointId) == "true")
		return nil
	})
	if !isMemberOfPoint {
		return
	}
	astPath := wm.app.Tools().Storage().StorageRoot() + "/machines/" + machineId + "/module"
	vmType := "wasm"
	wm.app.ModifyState(true, func(trx trx.ITrx) error {
		vm := model.Vm{MachineId: machineId}.Pull(trx)
		if vm.Path != "" {
			astPath = vm.Path
		}
		if vm.Runtime != "" {
			vmType = vm.Runtime
		}
		return nil
	})
	b, _ := json.Marshal(updates_points.Send{User: model.User{}, Point: point, Action: "single", Data: data})
	input := string(b)
	str, _ := json.Marshal(map[string]any{
		"type":      "runVm",
		"machineId": machineId,
		"input":     input,
		"astPath":   astPath,
		"vmType":    vmType,
	})
	wm.aeSocket <- string(str)
}

func (wm *Vmm) TerminateVm(machineId string) {
	str, _ := json.Marshal(map[string]any{
		"type":      "terminateVm",
		"machineId": machineId,
	})
	wm.aeSocket <- string(str)
}

func (wm *Vmm) BuildVmImage(machineId string, imageName string, dockerfilePath string) {
	str, _ := json.Marshal(map[string]any{
		"type":           "buildVmImage",
		"machineId":      machineId,
		"imageName":      imageName,
		"dockerfilePath": dockerfilePath,
	})
	wm.aeSocket <- string(str)
}

// WasmCallback is implemented in hostcall_global.go to keep host-call routing
// separated from runtime bootstrapping logic in this file.

func (wm *Vmm) handleRunDocker(input map[string]any, reqId int64) (string, int64) {
	machineId, err := checkField(input, "machineId", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	pointId, err := checkField(input, "pointId", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	found := false
	wm.app.ModifyState(true, func(trx trx.ITrx) error {
		if trx.GetLink("member::"+pointId+"::"+machineId) == "true" {
			found = true
		}
		return nil
	})
	if !found {
		err := errors.New("access denied")
		println(err)
		return err.Error(), reqId
	}
	inputFilesStr, err := checkField(input, "inputFiles", "{}")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	inputFiles := map[string]string{}
	err = json.Unmarshal([]byte(inputFilesStr), &inputFiles)
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	finalInputFiles := map[string]string{}
	for k, v := range inputFiles {
		if !wm.file.CheckFileFromStorage(wm.storageRoot, pointId, k) {
			err := errors.New("input file does not exist")
			println(err)
			return err.Error(), reqId
		}
		path := fmt.Sprintf("%s/files/%s/%s", wm.storageRoot, pointId, k)
		finalInputFiles[path] = v
	}
	imageName, err := checkField(input, "imageName", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	containerName, err := checkField(input, "containerName", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	isAsync, _ := checkField(input, "async", false)
	creatorUserId, _ := checkField(input, "creatorUserId", "")
	creatorSignature, _ := checkField(input, "creatorSignature", "")
	lockId, _ := checkField(input, "lockId", "")

	if creatorUserId == "" || creatorSignature == "" || lockId == "" {
		legacy, legacyErr := checkField(input, "containerMeta", "")
		if legacyErr != nil {
			legacy, legacyErr = checkField(input, "containerName", "")
		}
		if legacyErr == nil {
			parts := strings.Split(legacy, "|")
			if len(parts) >= 5 {
				containerName = parts[0]
				isAsync = parts[1] == "true"
				creatorUserId = parts[2]
				creatorSignature = parts[3]
				lockId = parts[4]
			}
		}
	}

	inp, _ := json.Marshal(inputs_users.ConsumeLockInput{
		Type:      "pay",
		UserId:    creatorUserId,
		Signature: creatorSignature,
		LockId:    lockId,
		Amount:    100,
	})
	sign := wm.app.SignPacketAsOwner(inp)
	resChan := make(chan bool)
	wm.app.ExecBaseRequestOnChain("/users/consumeLock", inp, sign, wm.app.OwnerId(), "", func(b []byte, i int, err error) {
		if err != nil {
			println(err)
			resChan <- false
		} else {
			resChan <- true
		}
	})
	if res := <-resChan; res {
		if isAsync {
			future.Async(func() {
				if imageName != "main" || containerName != "main" {
					wm.docker.Assign(machineId + "_" + imageName + "_" + containerName)
				}
				wm.docker.SaRContainer(machineId, imageName, containerName)
				wm.docker.RunContainer(machineId, pointId, imageName, containerName, finalInputFiles, false)
			}, false)
		} else {
			wm.docker.SaRContainer(machineId, imageName, containerName)
			outputFile, err := wm.docker.RunContainer(machineId, pointId, imageName, containerName, finalInputFiles, false)
			if err != nil {
				println(err)
				return err.Error(), reqId
			}
			if outputFile != nil {
				str, err := json.Marshal(outputFile)
				if err != nil {
					println(err)
					return err.Error(), reqId
				}
				return string(str), reqId
			}
		}
	}
	return "{}", reqId
}

func (wm *Vmm) handleExecDocker(input map[string]any, reqId int64) (string, int64) {
	machineId, err := checkField(input, "machineId", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	imageName, err := checkField(input, "imageName", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	containerName, err := checkField(input, "containerName", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	command, err := checkField(input, "command", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	output, err := wm.docker.ExecContainer(machineId, imageName, containerName, command)
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	return output, reqId
}

func (wm *Vmm) handleCopyToDocker(input map[string]any, reqId int64) (string, int64) {
	machineId, err := checkField(input, "machineId", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	imageName, err := checkField(input, "imageName", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	containerName, err := checkField(input, "containerName", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	fileName, err := checkField(input, "fileName", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	content, err := checkField(input, "content", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	err = wm.docker.CopyToContainer(machineId, imageName, containerName, fileName, content)
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	return "", reqId
}

func (wm *Vmm) handleHTTPPost(input map[string]any, reqId int64) (string, int64) {
	url, err := checkField(input, "url", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	method, _ := checkField(input, "method", "")
	if method == "" {
		method = strings.Split(url, "|")[0]
		url = url[len(method)+1:]
	}
	headers, err := checkField(input, "headers", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	body, err := checkField(input, "body", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	req, err := http.NewRequest(method, url, bytes.NewBuffer([]byte(body)))
	if err != nil {
		println("Error creating request:" + err.Error())
		return err.Error(), reqId
	}
	heads := map[string]string{}
	err = json.Unmarshal([]byte(headers), &heads)
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	for k, v := range heads {
		req.Header.Set(k, v)
	}
	client := &http.Client{}
	resp, err := client.Do(req)
	if err != nil {
		println("Request failed:" + err.Error())
		return err.Error(), reqId
	}
	defer resp.Body.Close()
	println("Response status:" + resp.Status)
	bodyBytes, err := io.ReadAll(resp.Body)
	if err != nil {
		println("Error reading response body:" + err.Error())
		return err.Error(), reqId
	}
	return base64.StdEncoding.EncodeToString(bodyBytes), reqId
}

func (wm *Vmm) handleCheckTokenValidity(input map[string]any, reqId int64) (string, int64) {
	tokenOwnerId, err := checkField(input, "tokenOwnerId", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	tokenId, err := checkField(input, "tokenId", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	gasLimit := int64(0)
	wm.app.ModifyState(true, func(trx trx.ITrx) error {
		if trx.GetString("Temp::User::"+tokenOwnerId+"::consumedTokens::"+tokenId) == "true" {
			return nil
		}
		if m, e := trx.GetJson("Json::User::"+tokenOwnerId, "lockedTokens."+tokenId); e == nil {
			gasLimit = int64(m["amount"].(float64))
		}
		return nil
	})
	jsn, _ := json.Marshal(map[string]any{"gasLimit": gasLimit})
	return string(jsn), reqId
}

func (wm *Vmm) handlePlantTrigger(input map[string]any, reqId int64) (string, int64) {
	count, err := checkField(input, "count", float64(0))
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	machineId, err := checkField(input, "machineId", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	tag, err := checkField(input, "tag", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	pointId, err := checkField(input, "pointId", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	data, err := checkField(input, "input", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	if tag == "alarm" {
		future.Async(func() {
			wm.app.ModifyState(false, func(trx trx.ITrx) error {
				trx.PutLink("vmAlarmPointId::"+machineId, pointId)
				trx.PutLink("vmAlarmData::"+machineId, data)
				trx.PutLink("vmAlarmTime::"+machineId, fmt.Sprintf("%d", time.Now().UnixMilli()+(int64(count)*1000)))
				return nil
			})
			time.Sleep(time.Duration(count) * time.Second)
			wm.app.ModifyState(false, func(trx trx.ITrx) error {
				trx.DelKey("link::vmAlarmPointId::" + machineId)
				trx.DelKey("link::vmAlarmData::" + machineId)
				trx.DelKey("link::vmAlarmTime::" + machineId)
				return nil
			})
			if wm.app.Tools().Security().HasAccessToPoint(machineId, pointId) {
				wm.RunVm(machineId, pointId, data)
			}
		}, false)
	} else {
		wm.app.PlantChainTrigger(int(count), machineId, tag, machineId, pointId, data)
	}
	return "{}", reqId
}

func (wm *Vmm) handleSignalPoint(input map[string]any, reqId int64) (string, int64) {
	machineId, err := checkField(input, "machineId", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	typAndTemp, err := checkField(input, "type", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	typ := typAndTemp
	temp := false
	if tempRaw, tempErr := checkField(input, "temp", false); tempErr == nil {
		temp = tempRaw
	} else {
		parts := strings.Split(typAndTemp, "|")
		typ = parts[0]
		if len(parts) > 1 {
			temp = parts[1] == "true"
		}
	}
	pointId, err := checkField(input, "pointId", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	userId, err := checkField(input, "userId", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	data, err := checkField(input, "data", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	wm.app.ModifyStateSecurly(false, base.NewInfo(machineId, pointId), func(s state.IState) error {
		_, _, err := wm.app.Actor().FetchAction("/points/signal").Act(s, inputs_points.SignalInput{
			Type:    typ,
			Data:    data,
			PointId: pointId,
			UserId:  userId,
			Temp:    temp,
		})
		return err
	})
	return "{}", reqId
}

func (wm *Vmm) handleRunVM(input map[string]any, reqId int64) (string, int64) {
	targetRuntime, err := checkField(input, "runtime", "")
	if err != nil {
		return err.Error(), reqId
	}
	targetMachineId, err := checkField(input, "machineId", "")
	if err != nil {
		return err.Error(), reqId
	}
	targetPointId, _ := checkField(input, "pointId", "")
	if targetRuntime == "wasm" {
		data, _ := checkField(input, "data", "{}")
		wm.RunVm(targetMachineId, targetPointId, data)
		return "{}", reqId
	}
	if targetRuntime == "docker" {
		return wm.handleRunDocker(input, reqId)
	}
	return "unsupported runtime", reqId
}

func parseChainReceivers(input map[string]any) map[string]map[string]bool {
	receivers := map[string]map[string]bool{}
	receiversRaw, ok := input["receivers"]
	if !ok {
		return map[string]map[string]bool{"*": map[string]bool{}}
	}
	nodesMap, ok := receiversRaw.(map[string]any)
	if !ok {
		return receivers
	}
	for nodeId, machineIdsRaw := range nodesMap {
		receivers[nodeId] = map[string]bool{}
		if machineIds, ok := machineIdsRaw.([]any); ok {
			for _, machineIdRaw := range machineIds {
				if machineId, ok := machineIdRaw.(string); ok {
					receivers[nodeId][machineId] = true
				}
			}
		}
	}
	if len(receivers) == 0 {
		receivers["*"] = map[string]bool{}
	}
	return receivers
}

func parseChainPayPacket(input map[string]any) *chain.ChainPayPacket {
	payRaw, ok := input["pay"]
	if !ok {
		return nil
	}
	payMap, ok := payRaw.(map[string]any)
	if !ok {
		return nil
	}
	pay := &chain.ChainPayPacket{}
	if v, ok := payMap["type"].(string); ok {
		pay.Type = v
	}
	if v, ok := payMap["sessionId"].(string); ok {
		pay.SessionId = v
	}
	if v, ok := payMap["userId"].(string); ok {
		pay.UserId = v
	}
	if v, ok := payMap["lockId"].(string); ok {
		pay.LockId = v
	}
	if v, ok := payMap["lockSignature"].(string); ok {
		pay.LockSignature = v
	}
	if v, ok := payMap["pointId"].(string); ok {
		pay.PointId = v
	}
	if v, ok := payMap["vmPayload"].(string); ok {
		pay.VmPayload = v
	}
	if v, ok := payMap["error"].(string); ok {
		pay.Error = v
	}
	if v, ok := payMap["amount"].(float64); ok {
		pay.Amount = int64(v)
	}
	if v, ok := payMap["requestedSeconds"].(float64); ok {
		pay.RequestedSeconds = int64(v)
	}
	if v, ok := payMap["acceptedSeconds"].(float64); ok {
		pay.AcceptedSeconds = int64(v)
	}
	if v, ok := payMap["costPerSecond"].(float64); ok {
		pay.CostPerSecond = int64(v)
	}
	if machineIdsRaw, ok := payMap["machineIds"].([]any); ok {
		pay.MachineIds = []string{}
		for _, m := range machineIdsRaw {
			if machineId, ok := m.(string); ok {
				pay.MachineIds = append(pay.MachineIds, machineId)
			}
		}
	}
	return pay
}

func (wm *Vmm) handleSendMessageOnChain(input map[string]any, reqId int64) (string, int64) {
	chainId, _ := checkField(input, "chainId", "main")
	key, err := checkField(input, "msgKey", "")
	if err != nil {
		key, err = checkField(input, "key", "")
		if err != nil {
			println(err)
			return err.Error(), reqId
		}
	}
	messageType, _ := checkField(input, "messageType", "vm.execute")
	payloadStr, _ := checkField(input, "payload", "{}")
	signature, _ := checkField(input, "signature", "")
	userId, _ := checkField(input, "userId", wm.app.OwnerId())
	replyTo, _ := checkField(input, "replyTo", "")
	pointId, _ := checkField(input, "pointId", "")
	receivers := parseChainReceivers(input)
	pay := parseChainPayPacket(input)
	wm.app.SendTypedMessageOnChain(chainId, key, messageType, []byte(payloadStr), signature, userId, receivers, replyTo, pointId, pay, nil)
	return "{}", reqId
}

func (wm *Vmm) handleTerminateVM(input map[string]any, reqId int64) (string, int64) {
	targetRuntime, err := checkField(input, "runtime", "")
	if err != nil {
		return err.Error(), reqId
	}
	targetMachineId, _ := checkField(input, "machineId", "")
	if targetRuntime == "docker" {
		imageName, _ := checkField(input, "imageName", "main")
		containerName, _ := checkField(input, "containerName", "main")
		wm.docker.SaRContainer(targetMachineId, imageName, containerName)
		return "{}", reqId
	}
	if targetRuntime == "wasm" {
		wm.TerminateVm(targetMachineId)
		return "{}", reqId
	}
	return "unsupported runtime", reqId
}

func checkField[T any](input map[string]any, fieldName string, defVal T) (T, error) {
	fRaw, ok := input[fieldName]
	if !ok {
		return defVal, errors.New("{\"error\":1}}")
	}
	f, ok := fRaw.(T)
	if !ok {
		return defVal, errors.New("{\"error\":2}}")
	}
	if newF, ok := fRaw.(string); ok {
		return any(string([]byte(newF))).(T), nil
	}
	return f, nil
}

func NewVmm(core core.ICore, storageRoot string, storage storage.IStorage, kvDbPath string, docker docker.IDocker, file file.IFile) *Vmm {
	os.MkdirAll(kvDbPath, os.ModePerm)
	wm := &Vmm{
		app:         core,
		storageRoot: storageRoot,
		storage:     storage,
		docker:      docker,
		file:        file,
		aeSocket:    make(chan string, 1000),
	}
	future.Async(func() {
		zctx, _ := zmq.NewContext()
		s, _ := zctx.NewSocket(zmq.REP)
		s.Bind("tcp://*:5555")

		zctx2, _ := zmq.NewContext()
		fmt.Printf("Connecting to the app engine server...\n")
		s2, _ := zctx2.NewSocket(zmq.REQ)
		s2.Connect("tcp://localhost:5556")

		future.Async(func() {
			for {
				msg := <-wm.aeSocket
				s2.Send(msg, 0)
				s2.Recv(0)
			}
		}, true)

		for {
			msg, _ := s.Recv(0)
			log.Printf("Received %s\n", msg)
			future.Async(func() {
				res, reqId := wm.WasmCallback(msg)
				result, _ := json.Marshal(map[string]any{
					"type":      "apiResponse",
					"requestId": reqId,
					"data":      res,
				})
				wm.aeSocket <- string(result)
			}, false)
			s.Send("", 0)
		}
	}, true)
	return wm
}

func (wm *Vmm) CloseKVDB() {
	// C.close()
}
