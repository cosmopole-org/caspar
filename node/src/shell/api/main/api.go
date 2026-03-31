package plugger_api

import (
	iaction "kasper/src/abstract/models/action"
	"kasper/src/abstract/models/core"
	"reflect"

	action_auth "kasper/src/shell/api/actions/auth"
	plugger_auth "kasper/src/shell/api/pluggers/auth"

	action_chain "kasper/src/shell/api/actions/chain"
	plugger_chain "kasper/src/shell/api/pluggers/chain"

	action_dummy "kasper/src/shell/api/actions/dummy"
	plugger_dummy "kasper/src/shell/api/pluggers/dummy"

	action_invite "kasper/src/shell/api/actions/invite"
	plugger_invite "kasper/src/shell/api/pluggers/invite"

	action_machine "kasper/src/shell/api/actions/machine"
	plugger_machine "kasper/src/shell/api/pluggers/machine"

	action_pc "kasper/src/shell/api/actions/pc"
	plugger_pc "kasper/src/shell/api/pluggers/pc"

	action_point "kasper/src/shell/api/actions/point"
	plugger_point "kasper/src/shell/api/pluggers/point"

	action_storage "kasper/src/shell/api/actions/storage"
	plugger_storage "kasper/src/shell/api/pluggers/storage"

	action_user "kasper/src/shell/api/actions/user"
	plugger_user "kasper/src/shell/api/pluggers/user"
)

func PlugThePlugger(core core.ICore, plugger interface{}) {
	s := reflect.TypeOf(plugger)
	for i := 0; i < s.NumMethod(); i++ {
		f := s.Method(i)
		if f.Name != "Install" {
			result := f.Func.Call([]reflect.Value{reflect.ValueOf(plugger)})
			action := result[0].Interface().(iaction.IAction)
			core.Actor().InjectAction(action)
		}
	}
}

func PlugAll(core core.ICore, modelExtender map[string]map[string]iaction.ExtendedField) {

	a_auth := &action_auth.Actions{App: core}
	p_auth := plugger_auth.New(a_auth, core)
	PlugThePlugger(core, p_auth)
	p_auth.Install(a_auth, modelExtender)

	a_chain := &action_chain.Actions{App: core}
	p_chain := plugger_chain.New(a_chain, core)
	PlugThePlugger(core, p_chain)
	p_chain.Install(a_chain, modelExtender)

	a_dummy := &action_dummy.Actions{App: core}
	p_dummy := plugger_dummy.New(a_dummy, core)
	PlugThePlugger(core, p_dummy)
	p_dummy.Install(a_dummy, modelExtender)

	a_invite := &action_invite.Actions{App: core}
	p_invite := plugger_invite.New(a_invite, core)
	PlugThePlugger(core, p_invite)
	p_invite.Install(a_invite, modelExtender)

	a_machine := &action_machine.Actions{App: core}
	p_machine := plugger_machine.New(a_machine, core)
	PlugThePlugger(core, p_machine)
	p_machine.Install(a_machine, modelExtender)

	a_pc := &action_pc.Actions{App: core}
	p_pc := plugger_pc.New(a_pc, core)
	PlugThePlugger(core, p_pc)
	p_pc.Install(a_pc, modelExtender)

	a_point := &action_point.Actions{App: core}
	p_point := plugger_point.New(a_point, core)
	PlugThePlugger(core, p_point)
	p_point.Install(a_point, modelExtender)

	a_storage := &action_storage.Actions{App: core}
	p_storage := plugger_storage.New(a_storage, core)
	PlugThePlugger(core, p_storage)
	p_storage.Install(a_storage, modelExtender)

	a_user := &action_user.Actions{App: core}
	p_user := plugger_user.New(a_user, core)
	PlugThePlugger(core, p_user)
	p_user.Install(a_user, modelExtender)

}
