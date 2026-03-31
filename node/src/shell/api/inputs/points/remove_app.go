package inputs_points

import "kasper/src/shell/utils/origin"

type RemoveMachineInput struct {
	MachineId string `json:"machineId" validate:"required"`
	PointId   string `json:"pointId" validate:"required"`
}

func (d RemoveMachineInput) GetData() any {
	return "dummy"
}

func (d RemoveMachineInput) GetPointId() string {
	return d.PointId
}

func (d RemoveMachineInput) Origin() string {
	return origin.FindOriginLocal(d.PointId)
}
