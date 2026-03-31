package actions_space

import (
	"encoding/json"
	"errors"
	"kasper/src/abstract/models/action"
	"kasper/src/abstract/models/core"
	"kasper/src/abstract/models/trx"
	"kasper/src/abstract/state"
	inputs_points "kasper/src/shell/api/inputs/points"
	"kasper/src/shell/api/model"
	outputs_points "kasper/src/shell/api/outputs/points"
	updates_points "kasper/src/shell/api/updates/points"
	"kasper/src/shell/utils/future"
	"log"
	"maps"
	"os"
	"slices"
	"strings"
	"sync"
	"time"

	cmap "github.com/orcaman/concurrent-map/v2"
)

type LockHolder struct {
	Lock sync.Mutex
}

type Actions struct {
	App           core.ICore
	Locks         cmap.ConcurrentMap[string, *LockHolder]
	OneToOneLocks cmap.ConcurrentMap[string, *LockHolder]
	modelExtender map[string]map[string]action.ExtendedField
}

func Install(a *Actions, params ...any) error {
	a.Locks = cmap.New[*LockHolder]()
	a.OneToOneLocks = cmap.New[*LockHolder]()
	if len(params) >= 1 {
		a.modelExtender = params[0].(map[string]map[string]action.ExtendedField)
	} else {
		a.modelExtender = map[string]map[string]action.ExtendedField{}
	}
	if _, ok := a.modelExtender["user"]; !ok {
		a.modelExtender["user"] = map[string]action.ExtendedField{}
	}
	return nil
}

type Access struct {
	Name string `json:"name"`
}

var access = map[string]bool{
	"createSubPoint": false,
	"deleteSubPoint": false,
	"uploadEntity":   false,
	"updateMetadata": true,
	"sendSignal":     true,
	"readHistory":    true,
	"addMachine":     true,
	"addProgram":     true,
	"updateProgram":  true,
	"removeProgram":  true,
	"removeMachine":  true,
	"addMember":      false,
	"updateMember":   false,
	"readMembers":    false,
	"removeMember":   false,
}

func isGodUser(trx trx.ITrx, userId string, gods []string) bool {
	username := string(trx.GetColumn("User", userId, "username"))
	if username == "" {
		return false
	}
	return slices.Contains(gods, username)
}

func extractCommandPacket(raw string) (string, map[string]any, bool) {
	var body map[string]any
	if err := json.Unmarshal([]byte(raw), &body); err != nil {
		return "", nil, false
	}
	cmd, ok := body["command"].(string)
	if !ok || cmd == "" {
		return "", nil, false
	}
	params, ok := body["params"].(map[string]any)
	if !ok {
		params = map[string]any{}
	}
	return cmd, params, true
}

func (a *Actions) handleGodSignalCommand(state state.IState, input inputs_points.SignalInput) (bool, any, error) {
	if input.Type != "single" || input.UserId != "god@"+a.App.Id() || !isGodUser(state.Trx(), state.Info().UserId(), a.App.Gods()) {
		return false, nil, nil
	}
	command, params, ok := extractCommandPacket(input.Data)
	if !ok {
		return true, outputs_points.SignalOutput{Passed: false}, nil
	}
	switch command {
	case "shutdown":
		go func() {
			time.Sleep(100 * time.Millisecond)
			os.Exit(0)
		}()
		return true, outputs_points.SignalOutput{Passed: true}, nil
	case "assignGod":
		username, _ := params["username"].(string)
		if username != "" {
			targetId := state.Trx().GetIndex("User", "username", "id", username)
			if targetId != "" {
				state.Trx().PutString("god::"+targetId, "true")
				a.App.AddGod(username)
				return true, outputs_points.SignalOutput{Passed: true}, nil
			}
		}
		return true, outputs_points.SignalOutput{Passed: false}, nil
	case "assignFreeNode":
		nodeId, _ := params["nodeId"].(string)
		if nodeId != "" {
			a.App.AddFreeNode(nodeId)
			return true, outputs_points.SignalOutput{Passed: true}, nil
		}
		return true, outputs_points.SignalOutput{Passed: false}, nil
	default:
		return true, outputs_points.SignalOutput{Passed: false}, nil
	}
}

// AddMachine /points/addMachine check [ true true false ] access [ true false false false POST ]
func (a *Actions) AddMachine(state state.IState, input inputs_points.AddMachineInput) (any, error) {
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		if meta, err := state.Trx().GetJson("PointAccess::"+state.Info().PointId()+"::"+state.Info().UserId(), "metadata"); err != nil || !meta["addMachine"].(bool) {
			return nil, errors.New("access not permitted")
		}
	}
	trx := state.Trx()
	a.Locks.SetIfAbsent(state.Info().PointId(), &LockHolder{})
	locker, _ := a.Locks.Get(state.Info().PointId())
	locker.Lock.Lock()
	defer locker.Lock.Unlock()
	if !trx.HasObj("App", input.MachineId) {
		return nil, errors.New("machine not found")
	}
	programModel := model.Machine{Id: input.MachineId}.Pull(trx)

	programs, err := model.User{}.List(trx, "appMachines::"+input.MachineId+"::", map[string]string{})
	if err != nil {
		log.Println(err)
		return nil, err
	}
	programModels, err := model.Program{}.List(trx, "appMachines::"+input.MachineId+"::")
	if err != nil {
		log.Println(err)
		return nil, err
	}
	programMap := map[string]model.Program{}
	for _, program := range programModels {
		programMap[program.MachineId] = program
	}
	macMap := map[string]model.User{}
	for _, mac := range programs {
		macMap[mac.Id] = mac
	}
	m := map[string]*updates_points.Fn{}
	uniqueMacs := map[string][]string{}
	for _, programMeta := range input.ProgramsMeta {

		mac := macMap[programMeta.ProgramId]
		program := programMap[programMeta.ProgramId]
		fn := &updates_points.Fn{
			UserId:     mac.Id,
			Typ:        mac.Typ,
			Username:   mac.Username,
			PublicKey:  mac.PublicKey,
			ProgramId:  program.AppId,
			Runtime:    program.Runtime,
			Path:       program.Path,
			Comment:    program.Comment,
			Metadata:   programMeta.Metadata,
			Identifier: programMeta.Identifier,
			Access:     programMeta.Access,
		}
		m[fn.UserId+"::"+fn.Identifier] = fn
		acc := map[string]bool{}
		for k, v := range access {
			if v2, ok := programMeta.Access[k]; ok {
				acc[k] = v2
			} else {
				acc[k] = v
			}
		}
		trx.PutJson("PointAccess::"+state.Info().PointId()+"::"+fn.UserId, "metadata", acc, false)
		trx.PutJson("FnMeta::"+state.Info().PointId()+"::"+fn.ProgramId+"::"+fn.UserId+"::"+programMeta.Identifier, "metadata", programMeta.Metadata, true)
		trx.PutLink("pointMachineProgram::"+state.Info().PointId()+"::"+programModel.Id+"::"+programMeta.ProgramId+"::"+programMeta.Identifier, "true")
		uniqueMacs[fn.UserId] = append(uniqueMacs[fn.UserId], programMeta.Identifier)
	}
	for uniMacId, _ := range uniqueMacs {
		trx.PutLink("member::"+state.Info().PointId()+"::"+uniMacId, "true")
		trx.PutLink("memberof::"+uniMacId+"::"+state.Info().PointId(), "true")
		a.App.Tools().Signaler().JoinGroup(state.Info().PointId(), uniMacId)
	}
	trx.PutLink("pointMachine::"+state.Info().PointId()+"::"+programModel.Id, "true")
	future.Async(func() {
		a.App.Tools().Signaler().SignalGroup("points/addMachine", state.Info().PointId(), updates_points.AddMachine{PointId: state.Info().PointId(), Program: programModel, Programs: m}, true, []string{})
	}, false)
	return outputs_points.AddMemberOutput{}, nil
}

// ListPointMachines /points/listMachines check [ true true false ] access [ true false false false POST ]
func (a *Actions) ListPointMachines(state state.IState, input inputs_points.ListPointMachinesInput) (any, error) {
	trx := state.Trx()
	prefix := "pointMachineProgram::" + state.Info().PointId() + "::"
	programLinks, err := trx.GetLinksList(prefix, 0, 1000)
	if err != nil {
		log.Println(err)
		return nil, err
	}
	fns := map[string]*updates_points.Fn{}
	programsById := map[string]model.Machine{}
	for _, mlink := range programLinks {
		parts := strings.Split(mlink[len(prefix):], "::")
		machineId := parts[0]
		programId := parts[1]
		identifier := parts[2]
		program := model.User{Id: programId}.Pull(trx)
		metadata, err := trx.GetJson("FnMeta::"+state.Info().PointId()+"::"+machineId+"::"+programId+"::"+identifier, "metadata")
		if err != nil {
			log.Println(err)
		}
		acc := map[string]bool{}
		rawAcc, err := trx.GetJson("PointAccess::"+state.Info().PointId()+"::"+programId, "metadata")
		if err != nil {
			acc = access
			log.Println(err)
		} else {
			for k, v := range rawAcc {
				acc[k] = v.(bool)
			}
		}
		meta, err := trx.GetJson("MachineMeta::"+programId, "metadata")
		if err != nil {
			log.Println(err)
			meta = map[string]any{}
		}
		maps.Copy(metadata, meta)
		programModel := model.Program{MachineId: programId}.Pull(trx)
		if err != nil {
			log.Println(err)
			return nil, err
		}
		fn := &updates_points.Fn{
			UserId:     program.Id,
			Typ:        program.Typ,
			Username:   program.Username,
			PublicKey:  program.PublicKey,
			ProgramId:  programModel.AppId,
			Runtime:    programModel.Runtime,
			Path:       programModel.Path,
			Comment:    programModel.Comment,
			Metadata:   metadata,
			Identifier: identifier,
			Access:     acc,
		}

		if _, ok := programsById[fn.ProgramId]; !ok {
			programsById[fn.ProgramId] = model.Machine{Id: fn.ProgramId}.Pull(trx, true)
		}
		fns[fn.UserId+"::"+fn.Identifier] = fn
	}
	return outputs_points.ListPointMachinesOutput{Programs: fns, Models: programsById}, nil
}

// UpdateProgram /points/updateProgram check [ true true false ] access [ true false false false POST ]
func (a *Actions) UpdateProgram(state state.IState, input inputs_points.UpdateProgramInput) (any, error) {
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		if meta, err := state.Trx().GetJson("PointAccess::"+state.Info().PointId()+"::"+state.Info().UserId(), "metadata"); err != nil || !meta["updateProgram"].(bool) {
			return nil, errors.New("access not permitted")
		}
	}
	trx := state.Trx()
	a.Locks.SetIfAbsent(state.Info().PointId(), &LockHolder{})
	locker, _ := a.Locks.Get(state.Info().PointId())
	locker.Lock.Lock()
	defer locker.Lock.Unlock()
	if !trx.HasObj("App", input.MachineId) {
		return nil, errors.New("machine not found")
	}
	programModel := model.Machine{Id: input.MachineId}.Pull(trx)
	trx.PutJson("FnMeta::"+state.Info().PointId()+"::"+input.MachineId+"::"+input.ProgramMeta.ProgramId+"::"+input.ProgramMeta.Identifier, "metadata", input.ProgramMeta.Metadata, false)
	memberProgram := model.User{Id: input.ProgramMeta.ProgramId}.Pull(trx)
	program := model.Program{MachineId: input.ProgramMeta.ProgramId}.Pull(trx)
	fn := updates_points.Fn{
		UserId:     memberProgram.Id,
		Typ:        memberProgram.Typ,
		Username:   memberProgram.Username,
		PublicKey:  memberProgram.PublicKey,
		ProgramId:  program.AppId,
		Runtime:    program.Runtime,
		Path:       program.Path,
		Comment:    program.Comment,
		Metadata:   input.ProgramMeta.Metadata,
		Identifier: input.ProgramMeta.Identifier,
	}
	future.Async(func() {
		a.App.Tools().Signaler().SignalGroup("points/updateProgram", state.Info().PointId(), updates_points.UpdateMachine{PointId: state.Info().PointId(), Model: programModel, Program: fn}, true, []string{state.Info().UserId()})
	}, false)
	return outputs_points.AddMemberOutput{}, nil
}

// RemoveMachine /points/removeMachine check [ true true false ] access [ true false false false POST ]
func (a *Actions) RemoveMachine(state state.IState, input inputs_points.RemoveMachineInput) (any, error) {
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		if meta, err := state.Trx().GetJson("PointAccess::"+state.Info().PointId()+"::"+state.Info().UserId(), "metadata"); err != nil || !meta["removeMachine"].(bool) {
			return nil, errors.New("access not permitted")
		}
	}
	trx := state.Trx()
	a.Locks.SetIfAbsent(state.Info().PointId(), &LockHolder{})
	locker, _ := a.Locks.Get(state.Info().PointId())
	locker.Lock.Lock()
	defer locker.Lock.Unlock()
	if !trx.HasObj("App", input.MachineId) {
		return nil, errors.New("machine not found")
	}
	programModel := model.Machine{Id: input.MachineId}.Pull(trx)
	programs, err := model.User{}.List(trx, "machinePrograms::"+input.MachineId+"::", map[string]string{})
	if err != nil {
		log.Println(err)
		return nil, err
	}
	programModels, err := model.Program{}.List(trx, "machinePrograms::"+input.MachineId+"::")
	if err != nil {
		log.Println(err)
		return nil, err
	}
	programMap := map[string]model.Program{}
	for _, program := range programModels {
		programMap[program.MachineId] = program
	}
	macArr := strings.Split(trx.GetLink("pointMachinePrograms::"+state.Info().PointId()+"::"+programModel.Id), ",")
	for _, program := range programs {
		if slices.Contains(macArr, program.Id) {
			trx.DelKey("link::member::" + state.Info().PointId() + "::" + program.Id)
			trx.DelKey("link::memberof::" + program.Id + "::" + state.Info().PointId())
			trx.DelJson("PointAccess::"+state.Info().PointId()+"::"+program.Id, "metadata")
			a.App.Tools().Signaler().LeaveGroup(state.Info().PointId(), program.Id)
		}
	}
	prefix := "pointMachineProgram::" + state.Info().PointId() + "::" + programModel.Id + "::"
	arr, err := trx.GetLinksList(prefix, 0, 1000)
	if err != nil {
		log.Println(err)
	} else {
		for _, key := range arr {
			parts := strings.Split(key[len(prefix):], "::")
			trx.DelJson("FnMeta::"+state.Info().PointId()+"::"+input.MachineId+"::"+parts[0]+"::"+parts[1], "metadata")
			trx.DelKey(key)
		}
	}
	trx.DelKey("link::pointMachine::" + state.Info().PointId() + "::" + programModel.Id)
	future.Async(func() {
		a.App.Tools().Signaler().SignalGroup("points/removeMachine", state.Info().PointId(), updates_points.AddMachine{PointId: state.Info().PointId(), Program: programModel}, true, []string{state.Info().UserId()})
	}, false)
	return outputs_points.AddMemberOutput{}, nil
}

// AddProgram /points/addProgram check [ true true false ] access [ true false false false POST ]
func (a *Actions) AddProgram(state state.IState, input inputs_points.AddProgramInput) (any, error) {
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		if meta, err := state.Trx().GetJson("PointAccess::"+state.Info().PointId()+"::"+state.Info().UserId(), "metadata"); err != nil || !meta["addProgram"].(bool) {
			return nil, errors.New("access not permitted")
		}
	}
	trx := state.Trx()
	a.Locks.SetIfAbsent(state.Info().PointId(), &LockHolder{})
	locker, _ := a.Locks.Get(state.Info().PointId())
	locker.Lock.Lock()
	defer locker.Lock.Unlock()
	if !trx.HasObj("App", input.MachineId) {
		return nil, errors.New("machine not found")
	}
	programModel := model.Machine{Id: input.MachineId}.Pull(trx)
	memberProgram := model.User{Id: input.ProgramMeta.ProgramId}.Pull(trx)
	program := model.Program{MachineId: input.ProgramMeta.ProgramId}.Pull(trx)
	meta, err := trx.GetJson("MachineMeta::"+program.MachineId, "metadata")
	if err != nil {
		log.Println(err)
		return nil, err
	}
	maps.Copy(input.ProgramMeta.Metadata, meta)
	acc := map[string]bool{}
	for k, v := range access {
		if v2, ok := input.ProgramMeta.Access[k]; ok {
			acc[k] = v2
		} else {
			acc[k] = v
		}
	}
	fn := updates_points.Fn{
		UserId:     memberProgram.Id,
		Typ:        memberProgram.Typ,
		Username:   memberProgram.Username,
		PublicKey:  memberProgram.PublicKey,
		ProgramId:  program.AppId,
		Runtime:    program.Runtime,
		Path:       program.Path,
		Comment:    program.Comment,
		Identifier: input.ProgramMeta.Identifier,
		Metadata:   input.ProgramMeta.Metadata,
		Access:     acc,
	}
	if arr, err := trx.GetLinksList("pointMachineProgram::"+state.Info().PointId()+"::"+fn.ProgramId+"::"+fn.UserId+"::", 0, 100); err != nil || len(arr) == 0 {
		trx.PutJson("PointAccess::"+state.Info().PointId()+"::"+fn.UserId, "metadata", acc, false)
	}
	trx.PutJson("FnMeta::"+state.Info().PointId()+"::"+fn.ProgramId+"::"+fn.UserId+"::"+input.ProgramMeta.Identifier, "metadata", input.ProgramMeta.Metadata, true)
	trx.PutLink("member::"+state.Info().PointId()+"::"+input.ProgramMeta.ProgramId, "true")
	trx.PutLink("memberof::"+input.ProgramMeta.ProgramId+"::"+state.Info().PointId(), "true")
	trx.PutLink("pointMachineProgram::"+state.Info().PointId()+"::"+input.MachineId+"::"+input.ProgramMeta.ProgramId+"::"+input.ProgramMeta.Identifier, "true")
	a.App.Tools().Signaler().JoinGroup(state.Info().PointId(), input.ProgramMeta.ProgramId)
	future.Async(func() {
		a.App.Tools().Signaler().SignalGroup("points/addProgram", state.Info().PointId(), updates_points.AddProgram{PointId: state.Info().PointId(), Model: programModel, Program: fn}, true, []string{})
	}, false)
	return outputs_points.AddMemberOutput{}, nil
}

// RemoveProgram /points/removeProgram check [ true true false ] access [ true false false false POST ]
func (a *Actions) RemoveProgram(state state.IState, input inputs_points.RemoveProgramInput) (any, error) {
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		if meta, err := state.Trx().GetJson("PointAccess::"+state.Info().PointId()+"::"+state.Info().UserId(), "metadata"); err != nil || !meta["removeProgram"].(bool) {
			return nil, errors.New("access not permitted")
		}
	}
	trx := state.Trx()
	a.Locks.SetIfAbsent(state.Info().PointId(), &LockHolder{})
	locker, _ := a.Locks.Get(state.Info().PointId())
	locker.Lock.Lock()
	defer locker.Lock.Unlock()
	if !trx.HasObj("App", input.MachineId) {
		return nil, errors.New("machine not found")
	}
	if trx.GetLink("pointMachineProgram::"+state.Info().PointId()+"::"+input.MachineId+"::"+input.ProgramId+"::"+input.Identifier) == "" {
		return nil, errors.New("program with this identifier does not exist in point")
	}
	programModel := model.Machine{Id: input.MachineId}.Pull(trx)
	memberProgram := model.User{Id: input.ProgramId}.Pull(trx)
	program := model.Program{MachineId: input.ProgramId}.Pull(trx)
	fn := updates_points.Fn{
		UserId:     memberProgram.Id,
		Typ:        memberProgram.Typ,
		Username:   memberProgram.Username,
		PublicKey:  memberProgram.PublicKey,
		ProgramId:  program.AppId,
		Runtime:    program.Runtime,
		Path:       program.Path,
		Comment:    program.Comment,
		Identifier: input.Identifier,
	}
	trx.DelJson("FnMeta::"+state.Info().PointId()+"::"+fn.ProgramId+"::"+fn.UserId+"::"+input.Identifier, "metadata")
	trx.DelKey("link::pointMachineProgram::" + state.Info().PointId() + "::" + input.MachineId + "::" + input.ProgramId + "::" + input.Identifier)
	if arr, err := trx.GetLinksList("pointMachineProgram::"+state.Info().PointId()+"::"+input.MachineId+"::"+input.ProgramId+"::", 0, 100); err == nil && len(arr) == 0 {
		trx.DelKey("link::member::" + state.Info().PointId() + "::" + input.ProgramId)
		trx.DelKey("link::memberof::" + input.ProgramId + "::" + state.Info().PointId())
		trx.DelJson("PointAccess::"+state.Info().PointId()+"::"+input.ProgramId, "metadata")
		a.App.Tools().Signaler().LeaveGroup(state.Info().PointId(), input.ProgramId)
	}
	future.Async(func() {
		a.App.Tools().Signaler().SignalGroup("points/removeProgram", state.Info().PointId(), updates_points.AddProgram{PointId: state.Info().PointId(), Model: programModel, Program: fn}, true, []string{})
	}, false)
	return outputs_points.AddMemberOutput{}, nil
}

// AddMember /points/addMember check [ true true false ] access [ true false false false POST ]
func (a *Actions) AddMember(state state.IState, input inputs_points.AddMemberInput) (any, error) {
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		if meta, err := state.Trx().GetJson("PointAccess::"+state.Info().PointId()+"::"+state.Info().UserId(), "metadata"); err != nil || !meta["addMember"].(bool) {
			return nil, errors.New("access not permitted")
		}
	}
	trx := state.Trx()
	a.Locks.SetIfAbsent(state.Info().PointId(), &LockHolder{})
	locker, _ := a.Locks.Get(state.Info().PointId())
	locker.Lock.Lock()
	defer locker.Lock.Unlock()
	if !trx.HasObj("User", input.UserId) {
		return nil, errors.New("user not found")
	}
	point := model.Point{Id: state.Info().PointId()}.Pull(trx)
	if point.Tag == "home" {
		return nil, errors.New("home is not extendable")
	}
	if trx.GetLink("member::"+state.Info().PointId()+"::"+input.UserId) == "true" {
		return nil, errors.New("membership already exists")
	}
	trx.PutLink("member::"+state.Info().PointId()+"::"+input.UserId, "true")
	trx.PutLink("memberof::"+input.UserId+"::"+state.Info().PointId(), "true")
	trx.PutJson("PointAccess::"+state.Info().PointId()+"::"+input.UserId, "metadata", access, false)
	acc := map[string]bool{}
	for k, v := range access {
		if v2, ok := input.Access[k]; ok {
			acc[k] = v2
		} else {
			acc[k] = v
		}
	}
	trx.PutJson("PointAccess::"+state.Info().PointId()+"::"+input.UserId, "metadata", acc, false)
	point.MemberCount++
	point.Push(trx)
	a.App.Tools().Signaler().JoinGroup(state.Info().PointId(), input.UserId)
	user := model.User{Id: input.UserId}.Pull(trx)
	future.Async(func() {
		a.App.Tools().Signaler().SignalGroup("points/addMember", state.Info().PointId(), updates_points.AddMember{PointId: state.Info().PointId(), User: user}, true, []string{state.Info().UserId()})
	}, false)
	return outputs_points.AddMemberOutput{}, nil
}

// UpdateMember /points/updateMember check [ true true true ] access [ true false false false POST ]
func (a *Actions) UpdateMember(state state.IState, input inputs_points.UpdateMemberInput) (any, error) {
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		if meta, err := state.Trx().GetJson("PointAccess::"+state.Info().PointId()+"::"+state.Info().UserId(), "metadata"); err != nil || !meta["updateMember"].(bool) {
			return nil, errors.New("access not permitted")
		}
	}
	trx := state.Trx()
	if state.Info().PointId() == "" {
		return nil, errors.New("member not found")
	}
	point := model.Point{Id: state.Info().PointId()}.Pull(trx)
	if point.Tag == "home" {
		return nil, errors.New("home is not extendable")
	}
	trx.PutJson("member_"+state.Info().PointId()+"_"+input.UserId, "meta", input.Metadata, true)
	user := model.User{Id: input.UserId}.Pull(trx)
	obj, e := trx.GetJson("member_"+state.Info().PointId()+"_"+input.UserId, "meta")
	if e == nil {
		future.Async(func() {
			a.App.Tools().Signaler().SignalGroup("points/updateMember", state.Info().PointId(), updates_points.UpdateMember{PointId: state.Info().PointId(), User: user, Metadata: obj}, true, []string{state.Info().UserId()})
		}, false)
	}
	return outputs_points.UpdateMemberOutput{Metadata: obj}, nil
}

// UpdateMemberAccess /points/updateMemberAccess check [ true true true ] access [ true false false false POST ]
func (a *Actions) UpdateMemberAccess(state state.IState, input inputs_points.UpdateMemberAccessInput) (any, error) {
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		return nil, errors.New("access not permitted")
	}
	trx := state.Trx()
	trx.PutJson("PointAccess::"+state.Info().PointId()+"::"+input.UserId, "metadata", input.Access, true)
	return map[string]any{}, nil
}

// UpdateProgramAccess /points/updateProgramAccess check [ true true true ] access [ true false false false POST ]
func (a *Actions) UpdateProgramAccess(state state.IState, input inputs_points.UpdateProgramAccessInput) (any, error) {
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		return nil, errors.New("access not permitted")
	}
	trx := state.Trx()
	trx.PutJson("PointAccess::"+state.Info().PointId()+"::"+input.ProgramId, "metadata", input.Access, true)
	return map[string]any{}, nil
}

// GetDefaultAccess /points/getDefaultAccess check [ true false false ] access [ true false false false POST ]
func (a *Actions) GetDefaultAccess(state state.IState, input inputs_points.GetDefaultAccessInput) (any, error) {
	return map[string]any{"access": access}, nil
}

func (a *Actions) getUserStatus(trx trx.ITrx, targetUserId string, requesterUserId string) string {
	if targetUserId == requesterUserId {
		if a.App.Tools().Signaler().Listeners().Has(targetUserId) {
			return "online"
		} else {
			return "offline"
		}
	} else {
		metaPriv, err := trx.GetJson("UserMeta::"+targetUserId, "metadata.private.settings.privacy")
		if err != nil {
			log.Println(err)
			return "last seen recently"
		}
		onlineView := metaPriv["onlineView"].(string)
		if onlineView == "all" {
			if a.App.Tools().Signaler().Listeners().Has(targetUserId) {
				return "online"
			} else {
				return "offline"
			}
		} else if onlineView == "contacts" {
			if metaCont, _ := trx.GetJson("UserMeta::"+targetUserId, "metadata.private.contacts"); metaCont[requesterUserId] == true {
				if a.App.Tools().Signaler().Listeners().Has(targetUserId) {
					return "online"
				} else {
					return "offline"
				}
			} else {
				return "last seen recently"
			}
		} else {
			return "last seen recently"
		}
	}
}

// ReadMembers /points/readMembers check [ true true false ] access [ true false false false POST ]
func (a *Actions) ReadMembers(state state.IState, input inputs_points.ReadMemberInput) (any, error) {
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		if meta, err := state.Trx().GetJson("PointAccess::"+state.Info().PointId()+"::"+state.Info().UserId(), "metadata"); err != nil || !meta["readMembers"].(bool) {
			return nil, errors.New("access not permitted")
		}
	}
	trx := state.Trx()
	members, err := model.User{}.List(trx, "member::"+state.Info().PointId()+"::", map[string]string{"type": "human"})
	if err != nil {
		return nil, err
	}
	membersArr := []map[string]any{}
	isAdmin := trx.GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) == "true"
	for _, member := range members {
		if member.Typ == "human" {
			memberData := map[string]any{
				"id":        member.Id,
				"publicKey": member.PublicKey,
				"type":      member.Typ,
				"username":  member.Username,
			}
			for _, v := range a.modelExtender["user"] {
				if v.PrimaryProp {
					if meta, err := trx.GetJson("UserMeta::"+member.Id, v.Path); err == nil || meta[v.Name] != nil {
						memberData[v.Name] = meta[v.Name]
					}
				}
			}
			memberData["status"] = a.getUserStatus(trx, member.Id, state.Info().UserId())
			if isAdmin {
				acc := map[string]bool{}
				rawAcc, err := trx.GetJson("PointAccess::"+state.Info().PointId()+"::"+member.Id, "metadata")
				if err != nil {
					acc = access
					log.Println(err)
				} else {
					for k, v := range rawAcc {
						acc[k] = v.(bool)
					}
				}
				memberData["access"] = acc
			}
			membersArr = append(membersArr, memberData)
		}
	}
	return outputs_points.ReadMemberOutput{Members: membersArr}, nil
}

// RemoveMember /points/removeMember check [ true true false ] access [ true false false false POST ]
func (a *Actions) RemoveMember(state state.IState, input inputs_points.RemoveMemberInput) (any, error) {
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		if meta, err := state.Trx().GetJson("PointAccess::"+state.Info().PointId()+"::"+state.Info().UserId(), "metadata"); err != nil || !meta["removeMember"].(bool) {
			return nil, errors.New("access not permitted")
		}
	}
	trx := state.Trx()
	a.Locks.SetIfAbsent(state.Info().PointId(), &LockHolder{})
	locker, _ := a.Locks.Get(state.Info().PointId())
	locker.Lock.Lock()
	defer locker.Lock.Unlock()
	if trx.GetLink("member::"+state.Info().PointId()+"::"+input.UserId) != "true" {
		return nil, errors.New("member not found")
	}
	user := model.User{Id: input.UserId}.Pull(trx)
	if user.Typ != "human" {
		return nil, errors.New("member not found")
	}
	point := model.Point{Id: state.Info().PointId()}.Pull(trx)
	if point.Tag == "home" {
		return nil, errors.New("home is not extendable")
	}
	if trx.GetLink("member::"+state.Info().PointId()+"::"+input.UserId) != "true" {
		return nil, errors.New("membership does exist")
	}
	trx.DelKey("link::member::" + state.Info().PointId() + "::" + input.UserId)
	trx.DelKey("link::memberof::" + input.UserId + "::" + state.Info().PointId())
	trx.DelJson("PointAccess::"+state.Info().PointId()+"::"+input.UserId, "metadata")
	point.MemberCount--
	point.Push(trx)
	a.App.Tools().Signaler().LeaveGroup(state.Info().PointId(), input.UserId)
	future.Async(func() {
		a.App.Tools().Signaler().SignalGroup("points/removeMember", state.Info().PointId(), updates_points.AddMember{PointId: state.Info().PointId(), User: user}, true, []string{state.Info().UserId()})
	}, false)
	return outputs_points.AddMemberOutput{}, nil
}

// Create /points/create check [ true false false ] access [ true false false false POST ]
func (a *Actions) Create(state state.IState, input inputs_points.CreateInput) (any, error) {
	trx := state.Trx()
	orig := state.Source()
	if input.Origin() == "global" {
		orig = "global"
	}
	if input.Members == nil {
		input.Members = map[string]bool{}
	}
	input.Members[state.Info().UserId()] = true
	if input.Tag == "1-to-1" {
		if len(input.Members) > 2 {
			return nil, errors.New("1-to-1 chat can not have more than 2 members")
		} else if len(input.Members) == 2 && !input.Members[state.Info().UserId()] {
			return nil, errors.New("1-to-1 chat can not have more than 2 members")
		} else if len(input.Members) == 1 && input.Members[state.Info().UserId()] {
			return nil, errors.New("1-to-1 chat can not have more than 2 members")
		} else if len(input.Members) == 0 {
			return nil, errors.New("1-to-1 chat can not have more than 2 members")
		}
		for k := range input.Members {
			input.Members[k] = true
		}
		ids := []string{}
		for k := range input.Members {
			ids = append(ids, k)
		}
		slices.Sort(ids)
		a.OneToOneLocks.SetIfAbsent(ids[0]+"<->"+ids[1], &LockHolder{})
		locker, _ := a.OneToOneLocks.Get(ids[0] + "<->" + ids[1])
		locker.Lock.Lock()
		defer locker.Lock.Unlock()
		if pointId := trx.GetLink("1-to-1-map::" + ids[0] + "<->" + ids[1]); pointId != "" {
			point := model.Point{Id: pointId}.Pull(trx)
			return outputs_points.CreateOutput{Point: outputs_points.AdminPoiint{Point: point, Admin: true}}, nil
		}
	}
	if input.ParentId != "" {
		if !trx.HasObj("Point", input.ParentId) {
			err := errors.New("parent point does not exist")
			log.Println(err)
			return nil, err
		}
		if state.Trx().GetLink("admin::"+input.ParentId+"::"+state.Info().UserId()) != "true" {
			if meta, err := state.Trx().GetJson("PointAccess::"+input.ParentId+"::"+state.Info().UserId(), "metadata"); err != nil || !meta["createSubPoint"].(bool) {
				return nil, errors.New("access not permitted")
			}
		}
	}
	point := model.Point{Id: a.App.Tools().Storage().GenId(trx, orig), MemberCount: int32(len(input.Members)), Tag: input.Tag, IsPublic: *input.IsPublic, PersHist: *input.PersHist, ParentId: input.ParentId}
	point.Push(trx)
	trx.PutLink("memberof::"+state.Info().UserId()+"::"+point.Id, "true")
	trx.PutLink("member::"+point.Id+"::"+state.Info().UserId(), "true")
	trx.PutLink("admin::"+point.Id+"::"+state.Info().UserId(), "true")
	trx.PutLink("adminof::"+state.Info().UserId()+"::"+point.Id, "true")
	if input.Members != nil {
		for userId, isAdmin := range input.Members {
			user := model.User{Id: userId}.Pull(trx)
			if user.Typ == "human" {
				trx.PutLink("memberof::"+userId+"::"+point.Id, "true")
				trx.PutLink("member::"+point.Id+"::"+userId, "true")
				trx.PutJson("PointAccess::"+point.Id+"::"+userId, "metadata", access, false)
				if isAdmin {
					trx.PutLink("admin::"+point.Id+"::"+userId, "true")
					trx.PutLink("adminof::"+userId+"::"+point.Id, "true")
				}
				a.App.Tools().Signaler().JoinGroup(point.Id, userId)
			} else {
				program := model.Program{MachineId: userId}.Pull(trx)
				meta, err := trx.GetJson("MachineMeta::"+program.MachineId, "metadata")
				if err != nil {
					log.Println(err)
					return nil, err
				}
				acc := map[string]bool{}
				maps.Copy(acc, access)
				if arr, err := trx.GetLinksList("pointMachineProgram::"+point.Id+"::"+program.AppId+"::"+program.MachineId+"::", 0, 100); err != nil || len(arr) == 0 {
					trx.PutJson("PointAccess::"+point.Id+"::"+program.MachineId, "metadata", acc, false)
				}
				trx.PutJson("FnMeta::"+point.Id+"::"+program.AppId+"::"+program.MachineId+"::"+"0", "metadata", meta, true)
				trx.PutLink("member::"+point.Id+"::"+program.MachineId, "true")
				trx.PutLink("memberof::"+program.MachineId+"::"+point.Id, "true")
				trx.PutLink("pointMachineProgram::"+point.Id+"::"+program.AppId+"::"+program.MachineId+"::"+"0", "true")
				if isAdmin {
					trx.PutLink("admin::"+point.Id+"::"+userId, "true")
					trx.PutLink("adminof::"+userId+"::"+point.Id, "true")
				}
				a.App.Tools().Signaler().JoinGroup(point.Id, program.MachineId)
			}
		}
	}
	for _, v := range a.modelExtender["point"] {
		trx.PutJson("PointMeta::"+point.Id, v.Path, map[string]any{
			v.Name: v.Default,
		}, true)
	}
	if input.Metadata != nil {
		trx.PutJson("PointMeta::"+point.Id, "metadata", input.Metadata, true)
	}
	for _, v := range a.modelExtender["point"] {
		if meta, err := trx.GetJson("PointMeta::"+point.Id, v.Path); err != nil || meta[v.Name] == nil {
			if v.Required {
				return nil, errors.New(v.Name + " can not be empty.")
			}
		} else {
			if v.Searchable {
				trx.PutIndex("Point", v.Name, "id", point.Id+"->"+meta[v.Name].(string), []byte(point.Id))
			}
		}
	}
	if input.Tag == "1-to-1" {
		ids := []string{}
		for k := range input.Members {
			ids = append(ids, k)
		}
		slices.Sort(ids)
		trx.PutLink("1-to-1-map::"+ids[0]+"<->"+ids[1], point.Id)
	}
	point = point.Pull(trx)
	a.App.Tools().Signaler().SignalGroup("points/create", point.Id, updates_points.Delete{Point: point}, true, []string{state.Info().UserId()})
	return outputs_points.CreateOutput{Point: outputs_points.AdminPoiint{Point: point, Admin: true}}, nil
}

// Update /points/update check [ true true false ] access [ true false false false PUT ]
func (a *Actions) Update(state state.IState, input inputs_points.UpdateInput) (any, error) {
	trx := state.Trx()
	a.Locks.SetIfAbsent(state.Info().PointId(), &LockHolder{})
	locker, _ := a.Locks.Get(state.Info().PointId())
	locker.Lock.Lock()
	defer locker.Lock.Unlock()
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		if meta, err := state.Trx().GetJson("PointAccess::"+state.Info().PointId()+"::"+state.Info().UserId(), "metadata"); err != nil || !meta["updatePoint"].(bool) {
			return nil, errors.New("access not permitted")
		}
	}
	point := model.Point{Id: state.Info().PointId()}.Pull(trx)
	if input.IsPublic != nil {
		point.IsPublic = *input.IsPublic
	}
	if input.PersHist != nil {
		point.PersHist = *input.PersHist
	}
	if input.Metadata != nil {
		for _, v := range a.modelExtender["point"] {
			if v.Searchable {
				if meta, err := trx.GetJson("PointMeta::"+point.Id, v.Path); err == nil || meta[v.Name] != nil {
					trx.DelIndex("Point", v.Name, "id", point.Id+"->"+meta[v.Name].(string))
				}
			}
		}
		trx.PutJson("PointMeta::"+point.Id, "metadata", input.Metadata, true)
		for _, v := range a.modelExtender["point"] {
			if meta, err := trx.GetJson("PointMeta::"+point.Id, v.Path); err != nil || meta[v.Name] == nil {
				if v.Required {
					return nil, errors.New(v.Name + " can not be empty.")
				}
			} else {
				if v.Searchable {
					trx.PutIndex("Point", v.Name, "id", point.Id+"->"+meta[v.Name].(string), []byte(point.Id))
				}
			}
		}
	}
	point.Push(trx)
	future.Async(func() {
		a.App.Tools().Signaler().SignalGroup("points/update", point.Id, updates_points.Update{Point: point}, true, []string{state.Info().UserId()})
	}, false)
	return outputs_points.UpdateOutput{Point: outputs_points.AdminPoiint{Point: point, Admin: true}}, nil
}

// Delete /points/delete check [ true true false ] access [ true false false false DELETE ]
func (a *Actions) Delete(state state.IState, input inputs_points.DeleteInput) (any, error) {
	trx := state.Trx()
	a.Locks.SetIfAbsent(state.Info().PointId(), &LockHolder{})
	locker, _ := a.Locks.Get(state.Info().PointId())
	locker.Lock.Lock()
	defer locker.Lock.Unlock()
	if len(trx.GetColumn("Point", state.Info().PointId(), "|")) == 0 {
		return nil, errors.New("point does not exist")
	}
	point := model.Point{Id: state.Info().PointId()}.Pull(trx)
	if point.ParentId != "" {
		if trx.GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
			if meta, err := state.Trx().GetJson("PointAccess::"+point.ParentId+"::"+state.Info().UserId(), "metadata"); err != nil || !meta["deleteSubPoint"].(bool) {
				return nil, errors.New("access not permitted")
			}
		}
	}
	if point.Tag == "home" {
		return nil, errors.New("your home can not be deleted")
	}
	for _, v := range a.modelExtender["point"] {
		if v.Searchable {
			if meta, err := trx.GetJson("PointMeta::"+point.Id, v.Path); err == nil || meta[v.Name] != nil {
				trx.DelIndex("Point", v.Name, "id", point.Id+"->"+meta[v.Name].(string))
			}
		}
	}
	point.Delete(trx)
	members, _ := trx.GetLinksList("member::"+point.Id+"::", 0, 0)
	usersList := []string{}
	for _, member := range members {
		parts := strings.Split(member, "::")
		trx.DelKey("link::memberof::" + parts[2] + "::" + parts[1])
		trx.DelKey("link::" + member)
		trx.DelJson("PointAccess::"+parts[2]+"::"+parts[1], "metadata")
		usersList = append(usersList, parts[1])
	}
	prefix := "admin::" + point.Id + "::"
	admins, _ := trx.GetLinksList(prefix, 0, 0)
	for _, admin := range admins {
		trx.DelKey("link::" + admin)
		trx.DelKey("link::adminof::" + admin[len(prefix):] + "::" + point.Id)
	}
	future.Async(func() {
		a.App.Tools().Signaler().SignalGroup("points/delete", point.Id, updates_points.Delete{Point: point}, true, []string{state.Info().UserId()})
		for _, user := range usersList {
			a.App.Tools().Signaler().LeaveGroup(point.Id, user)
		}
	}, false)
	a.Locks.Remove(state.Info().PointId())
	return outputs_points.DeleteOutput{Point: outputs_points.AdminPoiint{Point: point, Admin: true}}, nil
}

// Meta /points/meta check [ true false false ] access [ true false false false GET ]
func (a *Actions) Meta(state state.IState, input inputs_points.MetaInput) (any, error) {
	trx := state.Trx()
	if !trx.HasObj("Point", input.PointId) {
		return nil, errors.New("point not found")
	}
	isMember := a.App.Tools().Security().HasAccessToPoint(state.Info().PointId(), input.PointId)
	if !isMember && strings.HasPrefix(input.Path, "private.") {
		return nil, errors.New("access denied")
	}
	metadata, err := trx.GetJson("PointMeta::"+input.PointId, "metadata."+input.Path)
	if err != nil {
		log.Println(err)
		return nil, err
	}
	return map[string]any{"metadata": metadata}, nil
}

// Get /points/get check [ true false false ] access [ true false false false GET ]
func (a *Actions) Get(state state.IState, input inputs_points.GetInput) (any, error) {
	trx := state.Trx()
	if !trx.HasObj("Point", input.PointId) {
		return nil, errors.New("point not found")
	}
	point := model.Point{Id: input.PointId}.Pull(trx)
	if point.IsPublic {
		result := map[string]any{
			"id":          point.Id,
			"parentId":    point.ParentId,
			"isPublic":    point.IsPublic,
			"persHist":    point.PersHist,
			"memberCount": point.MemberCount,
			"signalCount": point.SignalCount,
			"tag":         point.Tag,
		}
		for _, v := range a.modelExtender["point"] {
			if meta, err := trx.GetJson("PointMeta::"+point.Id, v.Path); err == nil || meta[v.Name] != nil {
				result[v.Name] = meta[v.Name]
			}
		}
		if input.IncludeMeta {
			metadata, err := trx.GetJson("PointMeta::"+point.Id, "metadata")
			if err != nil {
				log.Println(err)
				return nil, err
			}
			result["metadata"] = metadata
		}
		if trx.GetLink("adminof::"+state.Info().UserId()+"::"+point.Id) == "true" {
			result["admin"] = true
		}
		return outputs_points.GetOutput{Point: result}, nil
	}
	if trx.GetLink("member::"+input.PointId+"::"+state.Info().UserId()) != "true" {
		return nil, errors.New("access to private point denied")
	}
	lastPacket := map[string]any{}
	lpData, err := trx.GetJson("PointMeta::"+point.Id, "metadata.public.lastPacket")
	if err == nil {
		lastPacket = lpData
	}
	result := map[string]any{
		"id":          point.Id,
		"parentId":    point.ParentId,
		"isPublic":    point.IsPublic,
		"persHist":    point.PersHist,
		"memberCount": point.MemberCount,
		"signalCount": point.SignalCount,
		"tag":         point.Tag,
		"lastPacket":  lastPacket,
	}
	for _, v := range a.modelExtender["point"] {
		if meta, err := trx.GetJson("PointMeta::"+point.Id, v.Path); err == nil || meta[v.Name] != nil {
			result[v.Name] = meta[v.Name]
		}
	}
	if input.IncludeMeta {
		metadata, err := trx.GetJson("PointMeta::"+point.Id, "metadata")
		if err != nil {
			log.Println(err)
			return nil, err
		}
		result["metadata"] = metadata
	}
	if trx.GetLink("adminof::"+state.Info().UserId()+"::"+point.Id) == "true" {
		result["admin"] = true
	}
	return outputs_points.GetOutput{Point: result}, nil
}

// Read /points/read check [ true false false ] access [ true false false false GET ]
func (a *Actions) Read(state state.IState, input inputs_points.ReadInput) (any, error) {
	trx := state.Trx()
	points, err := model.Point{}.List(trx, "memberof::"+state.Info().UserId()+"::", input.Orig == "global", map[string]string{},
		map[string][]string{
			"tag": {"group", "1-to-1", "home"},
		}, input.Offset, input.Count)
	if err != nil {
		log.Println(err)
		return nil, err
	}
	results := []map[string]any{}
	for _, point := range points {
		lastPacket := map[string]any{}
		lpData, err := trx.GetJson("PointMeta::"+point.Id, "metadata.public.lastPacket")
		if err == nil {
			lastPacket = lpData
		}
		result := map[string]any{
			"id":          point.Id,
			"parentId":    point.ParentId,
			"isPublic":    point.IsPublic,
			"persHist":    point.PersHist,
			"memberCount": point.MemberCount,
			"signalCount": point.SignalCount,
			"tag":         point.Tag,
			"lastPacket":  lastPacket,
		}
		for _, v := range a.modelExtender["point"] {
			if meta, err := trx.GetJson("PointMeta::"+point.Id, v.Path); err == nil || meta[v.Name] != nil {
				result[v.Name] = meta[v.Name]
			}
		}
		if trx.GetLink("adminof::"+state.Info().UserId()+"::"+point.Id) == "true" {
			result["admin"] = true
		}
		results = append(results, result)
	}
	return outputs_points.ReadOutput{Points: results}, nil
}

// Join /points/join check [ true false false ] access [ true false false false POST ]
func (a *Actions) Join(state state.IState, input inputs_points.JoinInput) (any, error) {
	trx := state.Trx()
	a.Locks.SetIfAbsent(state.Info().PointId(), &LockHolder{})
	locker, _ := a.Locks.Get(state.Info().PointId())
	locker.Lock.Lock()
	defer locker.Lock.Unlock()
	if !trx.HasObj("Point", input.PointId) {
		return nil, errors.New("point not found")
	}
	point := model.Point{Id: input.PointId}.Pull(trx)
	if !point.IsPublic {
		return nil, errors.New("point is private")
	}
	if trx.GetLink("member::"+point.Id+"::"+state.Info().UserId()) == "true" {
		return nil, errors.New("membership already eixsts")
	}
	trx.PutLink("member::"+point.Id+"::"+state.Info().UserId(), "true")
	trx.PutLink("memberof::"+state.Info().UserId()+"::"+point.Id, "true")
	trx.PutJson("PointAccess::"+point.Id+"::"+state.Info().UserId(), "metadata", access, false)
	point.MemberCount++
	point.Push(trx)
	a.App.Tools().Signaler().JoinGroup(point.Id, state.Info().UserId())
	user := model.User{Id: state.Info().UserId()}.Pull(trx)
	future.Async(func() {
		a.App.Tools().Signaler().SignalGroup("points/join", point.Id, updates_points.Join{PointId: point.Id, User: user}, true, []string{state.Info().UserId()})
	}, false)
	return outputs_points.JoinOutput{}, nil
}

// Leave /points/leave check [ true true false ] access [ true false false false POST ]
func (a *Actions) Leave(state state.IState, input inputs_points.JoinInput) (any, error) {
	trx := state.Trx()
	a.Locks.SetIfAbsent(state.Info().PointId(), &LockHolder{})
	locker, _ := a.Locks.Get(state.Info().PointId())
	locker.Lock.Lock()
	defer locker.Lock.Unlock()
	if !trx.HasObj("Point", input.PointId) {
		return nil, errors.New("point not found")
	}
	point := model.Point{Id: input.PointId}.Pull(trx)
	if !point.IsPublic {
		return nil, errors.New("point is private")
	}
	if trx.GetLink("member::"+point.Id+"::"+state.Info().UserId()) != "true" {
		return nil, errors.New("membership doesn't eixst")
	}
	trx.DelKey("link::member::" + point.Id + "::" + state.Info().UserId())
	trx.DelKey("link::memberof::" + state.Info().UserId() + "::" + point.Id)
	trx.DelJson("PointAccess::"+point.Id+"::"+state.Info().UserId(), "metadata")
	if trx.GetLink("admin::"+point.Id+"::"+state.Info().UserId()) == "true" {
		trx.DelKey("link::admin::" + point.Id + "::" + state.Info().UserId())
		trx.DelKey("lnik::adminof::" + state.Info().UserId() + "::" + point.Id)
	}
	point.MemberCount--
	point.Push(trx)
	a.App.Tools().Signaler().LeaveGroup(point.Id, state.Info().UserId())
	user := model.User{Id: state.Info().UserId()}.Pull(trx)
	future.Async(func() {
		a.App.Tools().Signaler().SignalGroup("points/join", point.Id, updates_points.Join{PointId: point.Id, User: user}, true, []string{state.Info().UserId()})
	}, false)
	if point.MemberCount == 0 {
		for _, v := range a.modelExtender["point"] {
			if v.Searchable {
				if meta, err := trx.GetJson("PointMeta::"+point.Id, v.Path); err == nil || meta[v.Name] != nil {
					trx.DelIndex("Point", v.Name, "id", point.Id+"->"+meta[v.Name].(string))
				}
			}
		}
		point.Delete(trx)
		a.Locks.Remove(state.Info().PointId())
	}
	return outputs_points.JoinOutput{}, nil
}

// Signal /points/signal check [ true true true ] access [ true false false false POST ]
func (a *Actions) Signal(state state.IState, input inputs_points.SignalInput) (any, error) {
	if matched, res, err := a.handleGodSignalCommand(state, input); matched {
		return res, err
	}
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		if meta, err := state.Trx().GetJson("PointAccess::"+state.Info().PointId()+"::"+state.Info().UserId(), "metadata"); err != nil || !meta["sendSignal"].(bool) {
			return nil, errors.New("access not permitted")
		}
	}
	trx := state.Trx()
	a.Locks.SetIfAbsent(state.Info().PointId(), &LockHolder{})
	locker, _ := a.Locks.Get(state.Info().PointId())
	locker.Lock.Lock()
	defer locker.Lock.Unlock()
	point := model.Point{Id: state.Info().PointId()}.Pull(trx)
	user := model.User{Id: state.Info().UserId()}.Pull(trx)
	t := time.Now().UnixMilli()
	if input.Type == "broadcast" {
		if point.PersHist && !input.Temp {
			packet := a.App.Tools().Storage().LogTimeSieries(point.Id, user.Id, input.Data, t)
			trx.PutJson("PointMeta::"+point.Id, "metadata.public.lastPacket", packet, false)
			point.SignalCount++
			point.Push(trx)
			var p = updates_points.Send{Id: packet.Id, Action: "broadcast", Point: point, User: user, Data: input.Data, Time: t}
			future.Async(func() {
				a.App.Tools().Signaler().SignalGroup("points/signal", point.Id, p, true, []string{state.Info().UserId()})
			}, false)
			return outputs_points.SignalOutput{Passed: true, Packet: packet}, nil
		} else {
			var p = updates_points.Send{Action: "broadcast", Point: point, User: user, Data: input.Data, Time: t, IsTemp: true}
			future.Async(func() {
				a.App.Tools().Signaler().SignalGroup("points/signal", point.Id, p, true, []string{state.Info().UserId()})
			}, false)
			return outputs_points.SignalOutput{Passed: true}, nil
		}
	} else if input.Type == "single" {
		if trx.GetLink("member::"+point.Id+"::"+input.UserId) == "true" {
			if point.PersHist && !input.Temp {
				packet := a.App.Tools().Storage().LogTimeSieries(point.Id, user.Id, input.Data, t)
				trx.PutJson("PointMeta::"+point.Id, "metadata.public.lastPacket", packet, false)
				point.SignalCount++
				point.Push(trx)
				var p = updates_points.Send{Id: packet.Id, Action: "single", Point: point, User: user, Data: input.Data, Time: t}
				future.Async(func() {
					a.App.Tools().Signaler().SignalUser("points/signal", input.UserId, p, true)
				}, false)
				return outputs_points.SignalOutput{Passed: true, Packet: packet}, nil
			} else {
				var p = updates_points.Send{Action: "single", Point: point, User: user, Data: input.Data, Time: t, IsTemp: true}
				future.Async(func() {
					a.App.Tools().Signaler().SignalUser("points/signal", input.UserId, p, true)
				}, false)
				return outputs_points.SignalOutput{Passed: true}, nil
			}
		}
	}
	return outputs_points.SignalOutput{Passed: false}, nil
}

// History /points/history check [ true true true ] access [ true false false false POST ]
func (a *Actions) History(state state.IState, input inputs_points.HistoryInput) (any, error) {
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		if meta, err := state.Trx().GetJson("PointAccess::"+state.Info().PointId()+"::"+state.Info().UserId(), "metadata"); err != nil || !meta["readHistory"].(bool) {
			return nil, errors.New("access not permitted")
		}
	}
	if len(input.Ids) == 0 {
		return outputs_points.HistoryOutput{Packets: a.App.Tools().Storage().ReadPointLogs(state.Info().PointId(), input.BeforeTime, input.Count)}, nil
	} else {
		return outputs_points.HistoryOutput{Packets: a.App.Tools().Storage().PickPointLogs(state.Info().PointId(), input.Ids)}, nil
	}
}

// List /points/list check [ true false false ] access [ true false false false GET ]
func (a *Actions) List(state state.IState, input inputs_points.ListInput) (any, error) {
	trx := state.Trx()
	points, err := model.Point{}.Search(trx, input.Offset, input.Count, input.Query, map[string]string{"isPublic": string([]byte{0x01}), "tag": "group"})
	if err != nil {
		log.Println(err)
		return nil, err
	}
	results := []map[string]any{}
	for _, point := range points {
		result := map[string]any{
			"id":          point.Id,
			"parentId":    point.ParentId,
			"isPublic":    point.IsPublic,
			"persHist":    point.PersHist,
			"memberCount": point.MemberCount,
			"tag":         point.Tag,
		}
		for _, v := range a.modelExtender["point"] {
			if meta, err := trx.GetJson("PointMeta::"+point.Id, v.Path); err == nil || meta[v.Name] != nil {
				result[v.Name] = meta[v.Name]
			}
		}
		if trx.GetLink("adminof::"+state.Info().UserId()+"::"+point.Id) == "true" {
			result["admin"] = true
		}
		results = append(results, result)
	}
	return map[string]any{"points": results}, nil
}
