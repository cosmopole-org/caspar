package updates_points

import "kasper/src/shell/api/model"

type Join struct {
	PointId string     `json:"pointId"`
	User    model.User `json:"user"`
}
