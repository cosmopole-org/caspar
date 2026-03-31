package outputs_points

import (
	"kasper/src/shell/api/model"
	updates_points "kasper/src/shell/api/updates/points"
)

type ListPointMachinesOutput struct {
	Programs map[string]*updates_points.Fn `json:"programs"`
	Models   map[string]model.Machine      `json:"models"`
}
