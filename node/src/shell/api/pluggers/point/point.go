package plugger_point

import (
	iaction "kasper/src/abstract/models/action"
	"kasper/src/abstract/models/core"
	actions "kasper/src/shell/api/actions/point"
	"kasper/src/shell/utils"
)

type Plugger struct {
	Id      *string
	Actions *actions.Actions
	Core    core.ICore
}

func (c *Plugger) AddMachine() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.AddMachine)
}

func (c *Plugger) ListPointMachines() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.ListPointMachines)
}

func (c *Plugger) UpdateProgram() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.UpdateProgram)
}

func (c *Plugger) RemoveMachine() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.RemoveMachine)
}

func (c *Plugger) AddProgram() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.AddProgram)
}

func (c *Plugger) RemoveProgram() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.RemoveProgram)
}

func (c *Plugger) AddMember() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.AddMember)
}

func (c *Plugger) UpdateMember() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.UpdateMember)
}

func (c *Plugger) UpdateMemberAccess() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.UpdateMemberAccess)
}

func (c *Plugger) UpdateProgramAccess() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.UpdateProgramAccess)
}

func (c *Plugger) GetDefaultAccess() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.GetDefaultAccess)
}

func (c *Plugger) ReadMembers() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.ReadMembers)
}

func (c *Plugger) RemoveMember() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.RemoveMember)
}

func (c *Plugger) Create() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.Create)
}

func (c *Plugger) Update() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.Update)
}

func (c *Plugger) Delete() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.Delete)
}

func (c *Plugger) Meta() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.Meta)
}

func (c *Plugger) Get() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.Get)
}

func (c *Plugger) Read() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.Read)
}

func (c *Plugger) Join() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.Join)
}

func (c *Plugger) Leave() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.Leave)
}

func (c *Plugger) Signal() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.Signal)
}

func (c *Plugger) History() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.History)
}

func (c *Plugger) List() iaction.IAction {
	return utils.ExtractSecureAction(c.Core, c.Actions.List)
}

func (c *Plugger) Install(a *actions.Actions, extra ...any) *Plugger {
	err := actions.Install(a, extra...)
	if err != nil {
		panic(err)
	}
	return c
}

func New(actions *actions.Actions, core core.ICore) *Plugger {
	id := "point"
	return &Plugger{Id: &id, Actions: actions, Core: core}
}
