package vmm

import "encoding/json"

// WasmCallback handles appengine host-call packets globally for all appengine
// runtimes (wasm/docker) through the same ZeroMQ callback channel.
func (wm *Vmm) WasmCallback(dataRaw string) (string, int64) {
	println(dataRaw)
	data := map[string]any{}
	err := json.Unmarshal([]byte(dataRaw), &data)
	if err != nil {
		println(err)
		return err.Error(), 0
	}
	reqIdRaw, err := checkField(data, "requestId", float64(0))
	if err != nil {
		println(err)
		return err.Error(), 0
	}
	reqId := int64(reqIdRaw)
	key, err := checkField(data, "key", "")
	if err != nil {
		println(err)
		return err.Error(), reqId
	}
	input, err := checkField[map[string]any](data, "input", nil)
	if err != nil {
		println(err)
		return err.Error(), reqId
	}

	switch key {
	case "execDocker", "execVm":
		return wm.handleExecDocker(input, reqId)
	case "copyToDocker", "copyToVm":
		return wm.handleCopyToDocker(input, reqId)
	case "httpPost":
		return wm.handleHTTPPost(input, reqId)
	case "checkTokenValidity":
		return wm.handleCheckTokenValidity(input, reqId)
	case "plantTrigger":
		return wm.handlePlantTrigger(input, reqId)
	case "signalPoint":
		return wm.handleSignalPoint(input, reqId)
	case "runVm":
		return wm.handleRunVM(input, reqId)
	case "terminateVm":
		return wm.handleTerminateVM(input, reqId)
	case "sendMessageOnChain":
		return wm.handleSendMessageOnChain(input, reqId)
	case "log":
		_, err := checkField(input, "text", "")
		if err != nil {
			println(err)
			return err.Error(), reqId
		}
	}

	return "{}", reqId
}
