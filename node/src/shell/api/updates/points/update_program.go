package updates_points

import "kasper/src/shell/api/model"

type UpdateMachine struct {
	PointId string        `json:"pointId"`
	Model   model.Machine `json:"program"`
	Program Fn            `json:"programData"`
}
