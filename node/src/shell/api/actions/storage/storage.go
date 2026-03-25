package actions_user

import (
	"bytes"
	"crypto/tls"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"image"
	"io"
	"io/ioutil"
	"kasper/src/abstract/models/core"
	"kasper/src/abstract/models/trx"
	"kasper/src/abstract/state"
	inputs_storage "kasper/src/shell/api/inputs/storage"
	models "kasper/src/shell/api/model"
	"kasper/src/shell/utils/future"
	"log"
	"maps"
	"math"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"

	_ "image/gif"
	_ "image/jpeg"
	_ "image/png"

	_ "golang.org/x/image/bmp"
	_ "golang.org/x/image/tiff"
	_ "golang.org/x/image/webp"
)

type Actions struct {
	App core.ICore
}

var imageTS = map[string]string{
	"avatar":     "200x200",
	"background": "1600x?",
}

func getImageDimensions(imgData []byte) (width, height int, err error) {
	cfg, _, err := image.DecodeConfig(bytes.NewReader(imgData))
	if err != nil {
		return 0, 0, err
	}
	return cfg.Width, cfg.Height, nil
}

func imageThumbSize(entityId string, data []byte) string {
	its := imageTS[entityId]
	if !strings.HasSuffix(its, "?") {
		return its
	}
	width, height, err := getImageDimensions(data)
	if err != nil {
		log.Println(err)
		targetWidth, _ := strconv.ParseInt(its[:len(its)-2], 10, 32)
		targetHeight := int(targetWidth) * 3 / 5
		return fmt.Sprintf("%dx%d", targetWidth, targetHeight)
	}
	targetWidth, _ := strconv.ParseInt(its[:len(its)-2], 10, 32)
	targetHeight := int(targetWidth) * height / width
	return fmt.Sprintf("%dx%d", targetWidth, targetHeight)
}

func imageQuality(byteLen int) string {
	return fmt.Sprintf("%d", int(math.Min(math.Max(10, math.Floor(500000/float64(byteLen)*100)), 100))) + "%"
}

func registerRoute(mux *http.ServeMux, path string, handler func(w http.ResponseWriter, r *http.Request)) {
	mux.HandleFunc(path, func(w http.ResponseWriter, r *http.Request) {
		defer func() {
			r := recover()
			if r != nil {
				var err error
				switch t := r.(type) {
				case string:
					err = errors.New(t)
				case error:
					err = t
				default:
					err = errors.New("unknown error")
				}
				log.Println(err)
				http.Error(w, err.Error(), http.StatusInternalServerError)
			}
		}()
		w.Header().Set("Access-Control-Allow-Origin", "*")
		w.Header().Set("Access-Control-Allow-Methods", "*")
		w.Header().Set("Access-Control-Allow-Headers", "*")
		w.Header().Set("Access-Control-Allow-Credentials", "true")

		if r.Method == http.MethodOptions {
			w.WriteHeader(http.StatusOK)
			return
		}
		handler(w, r)
	})
}

func Install(a *Actions, extra ...any) error {
	mux := http.NewServeMux()
	portStr := os.Getenv("ENTITY_API_PORT")
	port, err := strconv.ParseInt(portStr, 10, 32)
	if err != nil {
		panic(err)
	}
	server := &http.Server{
		Addr:      fmt.Sprintf(":%d", port),
		Handler:   mux,
		TLSConfig: a.App.Tools().Network().TlsConfig(),
	}
	registerRoute(mux, "/storage/downloadUserEntity", func(w http.ResponseWriter, r *http.Request) {
		userId := r.Header.Get("User-Id")
		inputLengthStr := r.Header.Get("Input-Length")
		ilI64, err := strconv.ParseInt(inputLengthStr, 10, 32)
		if err != nil {
			log.Printf("Error reading body: %v", err)
			http.Error(w, "can't read body", http.StatusBadRequest)
			return
		}
		inputLength := int(ilI64)
		body, err := ioutil.ReadAll(r.Body)
		if err != nil {
			log.Printf("Error reading body: %v", err)
			http.Error(w, "can't read body", http.StatusBadRequest)
			return
		}
		inputBody := body[0:inputLength]
		signature := body[inputLength:]
		if success, _, _ := a.App.Tools().Security().AuthWithSignature(userId, inputBody, string(signature)); !success {
			http.Error(w, "signature verification failed", http.StatusForbidden)
			return
		}
		origin := ""
		a.App.ModifyState(true, func(trx trx.ITrx) error {
			uParts := strings.Split(string(trx.GetColumn("User", userId, "username")), "@")
			if len(uParts) < 2 {
				return nil
			}
			origin = uParts[1]
			return nil
		})
		if origin == a.App.Id() {
			var input inputs_storage.DownloadUserEntityInput
			err = json.Unmarshal(inputBody, &input)
			if err != nil {
				log.Printf("Error parsing body: %v", err)
				http.Error(w, "can't parse body", http.StatusBadRequest)
				return
			}
			allowDownload := true
			a.App.ModifyState(true, func(trx trx.ITrx) error {
				if trx.HasObj("Vm", input.UserId) {
					flag := trx.GetLink("vmEntityDownloadable::" + input.UserId + "::" + input.EntityId)
					if flag != "true" {
						allowDownload = false
					}
				}
				return nil
			})
			if !allowDownload {
				http.Error(w, "entity is not downloadable", http.StatusForbidden)
				return
			}
			storagePath := a.App.Tools().Storage().StorageRoot() + "/entities/users/" + input.UserId
			storageFile := input.EntityId
			a.App.ModifyState(true, func(trx trx.ITrx) error {
				if trx.HasObj("Vm", input.UserId) {
					if entityPath := trx.GetLink("vmEntityPath::" + input.UserId + "::" + input.EntityId); entityPath != "" {
						storagePath = filepath.Dir(entityPath)
						storageFile = filepath.Base(entityPath)
					}
				}
				return nil
			})
			data, err := a.App.Tools().File().ReadFileFromGlobalStorage(storagePath, storageFile)
			if err != nil {
				log.Println(err)
				http.Error(w, "can't read file", http.StatusBadRequest)
				return
			}
			w.Write([]byte(data))
		} else {
			r.Body = ioutil.NopCloser(bytes.NewReader(body))
			url := fmt.Sprintf("%s://%s%s", "https", origin+":3000", r.RequestURI)
			proxyReq, err := http.NewRequest(r.Method, url, bytes.NewReader(body))
			if err != nil {
				http.Error(w, err.Error(), http.StatusInternalServerError)
				return
			}
			proxyReq.Header = make(http.Header)
			for h, val := range r.Header {
				proxyReq.Header[h] = val
			}
			httpClient := http.Client{}
			resp, err := httpClient.Do(proxyReq)
			if err != nil {
				http.Error(w, err.Error(), http.StatusBadGateway)
				return
			}
			defer resp.Body.Close()
			resp.Write(w)
		}
	})
	registerRoute(mux, "/storage/uploadUserEntity", func(w http.ResponseWriter, r *http.Request) {
		userId := r.Header.Get("User-Id")
		inputStr := r.Header.Get("Input")
		signature := r.Header.Get("Signature")
		if success, _, _ := a.App.Tools().Security().AuthWithSignature(userId, []byte(inputStr), signature); !success {
			http.Error(w, "signature verification failed", http.StatusForbidden)
			return
		}
		var input inputs_storage.UploadUserEntityInput
		err := json.Unmarshal([]byte(inputStr), &input)
		if err != nil {
			log.Printf("Error parsing body: %v", err)
			http.Error(w, "can't parse body", http.StatusBadRequest)
			return
		}
		data, err := ioutil.ReadAll(r.Body)
		if err != nil {
			log.Printf("Error reading body: %v", err)
			http.Error(w, "can't read body", http.StatusBadRequest)
			return
		}
		var e error
		a.App.ModifyState(false, func(trx trx.ITrx) error {
			if input.MachineId == "" {
				if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/users/"+userId, data, input.EntityId+".original", true); err != nil {
					log.Println(err)
					e = err
					return err
				}
				if mimeType := http.DetectContentType(data); strings.HasPrefix(mimeType, "image/") {
					entityPath := a.App.Tools().Storage().StorageRoot() + "/entities/users/" + userId + "/" + input.EntityId
					cmd := exec.Command("convert", entityPath+".original", "-quality", imageQuality(len(data)), "-thumbnail", imageThumbSize(input.EntityId, data)+">", entityPath+".jpg")
					output, err := cmd.Output()
					if err != nil {
						log.Fatalf("Command execution failed: %v", err)
					}
					fmt.Printf("Command output:\n%s", output)
				} else {
					if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/users/"+userId, data, input.EntityId, true); err != nil {
						log.Println(err)
						e = err
						return err
					}
				}
			} else {
				vm := models.Vm{MachineId: input.MachineId}.Pull(trx)
				app := models.App{Id: vm.AppId}.Pull(trx)
				if app.OwnerId != userId {
					e = errors.New("you are not owner of this machine")
					return err
				}
				if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/users/"+vm.MachineId, data, input.EntityId+".original", true); err != nil {
					log.Println(err)
					e = err
					return err
				}
				if mimeType := http.DetectContentType(data); strings.HasPrefix(mimeType, "image/") {
					entityPath := a.App.Tools().Storage().StorageRoot() + "/entities/users/" + vm.MachineId + "/" + input.EntityId
					cmd := exec.Command("convert", entityPath+".original", "-quality", imageQuality(len(data)), "-thumbnail", imageThumbSize(input.EntityId, data)+">", entityPath+".jpg")
					output, err := cmd.Output()
					if err != nil {
						log.Fatalf("Command execution failed: %v", err)
					}
					fmt.Printf("Command output:\n%s", output)
				} else {
					if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/users/"+vm.MachineId, data, input.EntityId, true); err != nil {
						log.Println(err)
						e = err
						return err
					}
				}
			}
			return nil
		})
		if e == nil {
			w.Write([]byte("{ \"resCode\": 0, \"obj\": {} }"))
		} else {
			b, _ := json.Marshal(map[string]any{
				"resCode": 1,
				"message": e.Error(),
			})
			w.Write(b)
		}
	})
	registerRoute(mux, "/storage/uploadPointEntity", func(w http.ResponseWriter, r *http.Request) {
		userId := r.Header.Get("User-Id")
		inputStr := r.Header.Get("Input")
		signature := r.Header.Get("Signature")
		if success, _, _ := a.App.Tools().Security().AuthWithSignature(userId, []byte(inputStr), signature); !success {
			http.Error(w, "signature verification failed", http.StatusForbidden)
			return
		}
		var input inputs_storage.UploadPointEntityInput
		err := json.Unmarshal([]byte(inputStr), &input)
		if err != nil {
			log.Printf("Error parsing body: %v", err)
			http.Error(w, "can't parse body", http.StatusBadRequest)
			return
		}
		origin := strings.Split(input.PointId, "@")[1]
		if origin == a.App.Id() || origin == "global" {
			data, err := ioutil.ReadAll(r.Body)
			if err != nil {
				log.Printf("Error reading body: %v", err)
				http.Error(w, "can't read body", http.StatusBadRequest)
				return
			}
			var e error
			a.App.ModifyState(false, func(trx trx.ITrx) error {
				if trx.GetLink("admin::"+input.PointId+"::"+userId) == "" {
					e = errors.New("you are not admin")
					return err
				}
				if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/points/"+input.PointId, data, input.EntityId+".original", true); err != nil {
					log.Println(err)
					e = err
					return err
				}
				if mimeType := http.DetectContentType(data); strings.HasPrefix(mimeType, "image/") {
					entityPath := a.App.Tools().Storage().StorageRoot() + "/entities/points/" + input.PointId + "/" + input.EntityId
					cmd := exec.Command("convert", entityPath+".original", "-quality", imageQuality(len(data)), "-thumbnail", imageThumbSize(input.EntityId, data)+">", entityPath+".jpg")
					output, err := cmd.Output()
					if err != nil {
						log.Fatalf("Command execution failed: %v", err)
					}
					fmt.Printf("Command output:\n%s", output)
				} else {
					if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/points/"+input.PointId, data, input.EntityId, true); err != nil {
						log.Println(err)
						e = err
						return err
					}
				}
				future.Async(func() {
					a.App.Tools().Signaler().SignalGroup("storage/updatePointEntity", input.PointId, map[string]any{"pointId": input.PointId, "entityId": input.EntityId}, true, []string{})
				}, false)
				return nil
			})
			if e == nil {
				w.Write([]byte("{ \"resCode\": 0, \"obj\": {} }"))
			} else {
				b, _ := json.Marshal(map[string]any{
					"resCode": 1,
					"message": e.Error(),
				})
				w.Write(b)
			}
		} else {
			url := fmt.Sprintf("%s://%s%s", "https", origin+":3000", r.RequestURI)
			proxyReq, err := http.NewRequest(r.Method, url, r.Body)
			if err != nil {
				http.Error(w, err.Error(), http.StatusInternalServerError)
				return
			}
			proxyReq.Header = make(http.Header)
			for h, val := range r.Header {
				proxyReq.Header[h] = val
			}
			httpClient := http.Client{}
			resp, err := httpClient.Do(proxyReq)
			if err != nil {
				http.Error(w, err.Error(), http.StatusBadGateway)
				return
			}
			defer resp.Body.Close()
			resp.Write(w)
		}
	})
	registerRoute(mux, "/storage/uploadAppEntity", func(w http.ResponseWriter, r *http.Request) {
		userId := r.Header.Get("User-Id")
		inputStr := r.Header.Get("Input")
		signature := r.Header.Get("Signature")
		if success, _, _ := a.App.Tools().Security().AuthWithSignature(userId, []byte(inputStr), signature); !success {
			http.Error(w, "signature verification failed", http.StatusForbidden)
			return
		}
		var input inputs_storage.UploadAppEntityInput
		err := json.Unmarshal([]byte(inputStr), &input)
		if err != nil {
			log.Printf("Error parsing body: %v", err)
			http.Error(w, "can't parse body", http.StatusBadRequest)
			return
		}
		data, err := ioutil.ReadAll(r.Body)
		if err != nil {
			log.Printf("Error reading body: %v", err)
			http.Error(w, "can't read body", http.StatusBadRequest)
			return
		}
		var e error
		a.App.ModifyState(false, func(trx trx.ITrx) error {
			app := models.App{Id: input.AppId}.Pull(trx)
			if app.OwnerId != userId {
				e = errors.New("you are not owner of app")
				return err
			}
			if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/apps/"+input.AppId, data, input.EntityId+".original", true); err != nil {
				log.Println(err)
				e = err
				return err
			}
			if mimeType := http.DetectContentType(data); strings.HasPrefix(mimeType, "image/") {
				entityPath := a.App.Tools().Storage().StorageRoot() + "/entities/apps/" + input.AppId + "/" + input.EntityId
				cmd := exec.Command("convert", entityPath+".original", "-quality", imageQuality(len(data)), "-thumbnail", imageThumbSize(input.EntityId, data)+">", entityPath+".jpg")
				output, err := cmd.Output()
				if err != nil {
					log.Fatalf("Command execution failed: %v", err)
				}
				fmt.Printf("Command output:\n%s", output)
			} else {
				if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/apps/"+input.AppId, data, input.EntityId, true); err != nil {
					log.Println(err)
					e = err
					return err
				}
			}
			return nil
		})
		if e == nil {
			w.Write([]byte("{ \"resCode\": 0, \"obj\": {} }"))
		} else {
			b, _ := json.Marshal(map[string]any{
				"resCode": 1,
				"message": e.Error(),
			})
			w.Write(b)
		}
	})
	registerRoute(mux, "/storage/downloadAppEntity", func(w http.ResponseWriter, r *http.Request) {
		userId := r.Header.Get("User-Id")
		inputLengthStr := r.Header.Get("Input-Length")
		ilI64, err := strconv.ParseInt(inputLengthStr, 10, 32)
		if err != nil {
			log.Printf("Error reading body: %v", err)
			http.Error(w, "can't read body", http.StatusBadRequest)
			return
		}
		inputLength := int(ilI64)
		var input inputs_storage.DownloadAppEntityInput
		body, err := ioutil.ReadAll(r.Body)
		if err != nil {
			log.Printf("Error reading body: %v", err)
			http.Error(w, "can't read body", http.StatusBadRequest)
			return
		}
		inputBody := body[0:inputLength]
		signature := body[inputLength:]
		if success, _, _ := a.App.Tools().Security().AuthWithSignature(userId, inputBody, string(signature)); !success {
			http.Error(w, "signature verification failed", http.StatusForbidden)
			return
		}
		err = json.Unmarshal(inputBody, &input)
		if err != nil {
			log.Printf("Error parsing body: %v", err)
			http.Error(w, "can't parse body", http.StatusBadRequest)
			return
		}
		data, err := a.App.Tools().File().ReadFileFromGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/apps/"+input.AppId, input.EntityId)
		if err != nil {
			log.Println(err)
			http.Error(w, "can't read file", http.StatusBadRequest)
			return
		}
		w.Write([]byte(data))
	})
	registerRoute(mux, "/storage/downloadPointEntity", func(w http.ResponseWriter, r *http.Request) {
		userId := r.Header.Get("User-Id")
		inputLengthStr := r.Header.Get("Input-Length")
		ilI64, err := strconv.ParseInt(inputLengthStr, 10, 32)
		if err != nil {
			log.Printf("Error reading body: %v", err)
			http.Error(w, "can't read body", http.StatusBadRequest)
			return
		}
		inputLength := int(ilI64)
		var input inputs_storage.DownloadPointEntityInput
		body, err := ioutil.ReadAll(r.Body)
		if err != nil {
			log.Printf("Error reading body: %v", err)
			http.Error(w, "can't read body", http.StatusBadRequest)
			return
		}
		inputBody := body[0:inputLength]
		signature := body[inputLength:]
		if success, _, _ := a.App.Tools().Security().AuthWithSignature(userId, inputBody, string(signature)); !success {
			http.Error(w, "signature verification failed", http.StatusForbidden)
			return
		}
		origin := ""
		a.App.ModifyState(true, func(trx trx.ITrx) error {
			uParts := strings.Split(string(trx.GetColumn("User", userId, "username")), "@")
			if len(uParts) < 2 {
				return nil
			}
			origin = uParts[1]
			return nil
		})
		if origin == a.App.Id() {
			err = json.Unmarshal(inputBody, &input)
			if err != nil {
				log.Printf("Error parsing body: %v", err)
				http.Error(w, "can't parse body", http.StatusBadRequest)
				return
			}
			data, err := a.App.Tools().File().ReadFileFromGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/points/"+input.PointId, input.EntityId)
			if err != nil {
				log.Println(err)
				http.Error(w, "can't read file", http.StatusBadRequest)
				return
			}
			w.Write([]byte(data))
		} else {
			r.Body = ioutil.NopCloser(bytes.NewReader(body))
			url := fmt.Sprintf("%s://%s%s", "https", origin+":3000", r.RequestURI)
			proxyReq, err := http.NewRequest(r.Method, url, bytes.NewReader(body))
			if err != nil {
				http.Error(w, err.Error(), http.StatusInternalServerError)
				return
			}
			proxyReq.Header = make(http.Header)
			for h, val := range r.Header {
				proxyReq.Header[h] = val
			}
			httpClient := http.Client{}
			resp, err := httpClient.Do(proxyReq)
			if err != nil {
				http.Error(w, err.Error(), http.StatusBadGateway)
				return
			}
			defer resp.Body.Close()
			resp.Write(w)
		}
	})
	registerRoute(mux, "/stream/get", func(w http.ResponseWriter, r *http.Request) {
		userId := r.URL.Query().Get("userId")
		inputBody := []byte(r.URL.Query().Get("input"))
		signature := r.URL.Query().Get("signature")
		if success, _, _ := a.App.Tools().Security().AuthWithSignature(userId, inputBody, string(signature)); !success {
			log.Println("Error accessing point:", err.Error())
			http.Error(w, "signature verification failed", http.StatusForbidden)
			return
		}
		origin := ""
		a.App.ModifyState(true, func(trx trx.ITrx) error {
			uParts := strings.Split(string(trx.GetColumn("User", userId, "username")), "@")
			if len(uParts) < 2 {
				return nil
			}
			origin = uParts[1]
			return nil
		})
		if origin == a.App.Id() {
			var input inputs_storage.StreamGetInput
			err = json.Unmarshal(inputBody, &input)
			if err != nil {
				log.Println("Error parsing body:", err.Error())
				http.Error(w, "can't parse body", http.StatusBadRequest)
				return
			}
			if !a.App.Tools().Security().HasAccessToPoint(userId, input.PointId) {
				log.Printf("Error accessing point: %v", err)
				http.Error(w, "can't access point", http.StatusForbidden)
				return
			}
			url := fmt.Sprintf("%s://%s%s", "https", "10.10.0.5:8443", "/"+strings.Join(strings.Split(input.MachineId, "@"), "_")+"/stream/get/")
			log.Println(url)
			proxyReq, err := http.NewRequest("POST", url, bytes.NewReader([]byte("{}")))
			if err != nil {
				log.Println(err)
				http.Error(w, err.Error(), http.StatusInternalServerError)
				return
			}
			proxyReq.Header = make(http.Header)
			maps.Copy(proxyReq.Header, r.Header)
			proxyReq.Header.Set("User-Id", userId)
			proxyReq.Header.Set("Point-Id", input.PointId)
			proxyReq.Header.Set("Metadata", input.Metadata)
			tr := &http.Transport{
				TLSClientConfig: &tls.Config{InsecureSkipVerify: true},
			}
			httpClient := &http.Client{Transport: tr}
			resp, err := httpClient.Do(proxyReq)
			if err != nil {
				log.Println(err)
				http.Error(w, err.Error(), http.StatusBadGateway)
				return
			}
			defer resp.Body.Close()
			io.Copy(w, resp.Body)
		} else {
			url := fmt.Sprintf("%s://%s%s", "https", origin+":3000", r.RequestURI)
			proxyReq, err := http.NewRequest(r.Method, url, bytes.NewReader([]byte("{}")))
			if err != nil {
				http.Error(w, err.Error(), http.StatusInternalServerError)
				return
			}
			proxyReq.Header = make(http.Header)
			for h, val := range r.Header {
				proxyReq.Header[h] = val
			}
			httpClient := http.Client{}
			resp, err := httpClient.Do(proxyReq)
			if err != nil {
				http.Error(w, err.Error(), http.StatusBadGateway)
				return
			}
			defer resp.Body.Close()
			resp.Write(w)
		}
	})
	registerRoute(mux, "/stream/send", func(w http.ResponseWriter, r *http.Request) {
		userId := r.URL.Query().Get("userId")
		inputBody := []byte(r.URL.Query().Get("input"))
		signature := r.URL.Query().Get("signature")
		if success, _, _ := a.App.Tools().Security().AuthWithSignature(userId, inputBody, string(signature)); !success {
			log.Println("Error accessing point:", err.Error())
			http.Error(w, "signature verification failed", http.StatusForbidden)
			return
		}
		origin := ""
		a.App.ModifyState(true, func(trx trx.ITrx) error {
			uParts := strings.Split(string(trx.GetColumn("User", userId, "username")), "@")
			if len(uParts) < 2 {
				return nil
			}
			origin = uParts[1]
			return nil
		})
		if origin == a.App.Id() {
			var input inputs_storage.StreamGetInput
			err = json.Unmarshal(inputBody, &input)
			if err != nil {
				log.Println("Error parsing body:", err.Error())
				http.Error(w, "can't parse body", http.StatusBadRequest)
				return
			}
			if !a.App.Tools().Security().HasAccessToPoint(userId, input.PointId) {
				log.Printf("Error accessing point: %v", err)
				http.Error(w, "can't access point", http.StatusForbidden)
				return
			}
			url := fmt.Sprintf("%s://%s%s", "https", "10.10.0.5:8443", "/"+strings.Join(strings.Split(input.MachineId, "@"), "_")+"/stream/send/")
			log.Println(url)

			body, err := ioutil.ReadAll(r.Body)
			if err != nil {
				log.Printf("Error reading body: %v", err)
				http.Error(w, "can't read body", http.StatusBadRequest)
				return
			}
			log.Println("len of body in proxy", len(body))
			proxyReq, err := http.NewRequest("POST", url, bytes.NewReader(body))
			if err != nil {
				log.Println(err)
				http.Error(w, err.Error(), http.StatusInternalServerError)
				return
			}
			proxyReq.Header = make(http.Header)
			maps.Copy(proxyReq.Header, r.Header)
			proxyReq.Header.Set("Content-Type", "application/octet-stream")
			proxyReq.Header.Set("User-Id", userId)
			proxyReq.Header.Set("Point-Id", input.PointId)
			proxyReq.Header.Set("Metadata", input.Metadata)
			tr := &http.Transport{
				TLSClientConfig: &tls.Config{InsecureSkipVerify: true},
			}
			httpClient := &http.Client{Transport: tr}
			resp, err := httpClient.Do(proxyReq)
			if err != nil {
				log.Println(err)
				http.Error(w, err.Error(), http.StatusBadGateway)
				return
			}
			defer resp.Body.Close()
			io.Copy(w, resp.Body)
		} else {
			url := fmt.Sprintf("%s://%s%s", "https", origin+":3000", r.RequestURI)
			proxyReq, err := http.NewRequest(r.Method, url, r.Body)
			if err != nil {
				http.Error(w, err.Error(), http.StatusInternalServerError)
				return
			}
			proxyReq.Header = make(http.Header)
			for h, val := range r.Header {
				proxyReq.Header[h] = val
			}
			httpClient := http.Client{}
			resp, err := httpClient.Do(proxyReq)
			if err != nil {
				http.Error(w, err.Error(), http.StatusBadGateway)
				return
			}
			defer resp.Body.Close()
			resp.Write(w)
		}
	})
	future.Async(func() {
		server.ListenAndServeTLS("", "")
	}, false)
	return nil
}

// Upload /storage/upload check [ true true true ] access [ true false false false POST ]
func (a *Actions) Upload(state state.IState, input inputs_storage.UploadDataInput) (any, error) {
	trx := state.Trx()
	if input.FileId != "" {
		if !trx.HasObj("File", input.FileId) {
			return nil, errors.New("file not found")
		}
		var file = models.File{Id: input.FileId}.Pull(trx)
		if file.OwnerId != state.Info().UserId() {
			return nil, errors.New("access to file control denied")
		}
		data, err := base64.StdEncoding.DecodeString(input.Data)
		if err != nil {
			log.Println(err)
			return nil, err
		}
		if err := a.App.Tools().File().SaveDataToStorage(a.App.Tools().Storage().StorageRoot(), data, state.Info().PointId(), input.FileId); err != nil {
			log.Println(err)
			return nil, err
		}
		return map[string]any{}, nil
	} else {
		var file = models.File{Id: a.App.Tools().Storage().GenId(trx, input.Origin()), OwnerId: state.Info().UserId(), PointId: state.Info().PointId()}
		data, err := base64.StdEncoding.DecodeString(input.Data)
		if err != nil {
			log.Println(err)
			return nil, err
		}
		if err := a.App.Tools().File().SaveDataToStorage(a.App.Tools().Storage().StorageRoot(), data, state.Info().PointId(), file.Id); err != nil {
			log.Println(err)
			return nil, err
		}
		file.Push(trx)
		return map[string]any{"file": file}, nil
	}
}

// UploadUserEntity /storage/uploadUserEntity check [ true false true ] access [ true false false false POST ]
func (a *Actions) UploadUserEntity(state state.IState, input inputs_storage.UploadUserEntityInput) (any, error) {
	trx := state.Trx()
	data, err := base64.StdEncoding.DecodeString(input.Data)
	if err != nil {
		log.Println(err)
		return nil, err
	}
	if input.MachineId == "" {
		if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/users/"+state.Info().UserId(), data, input.EntityId+".original", true); err != nil {
			log.Println(err)
			return nil, err
		}
		if mimeType := http.DetectContentType(data); strings.HasPrefix(mimeType, "image/") {
			entityPath := a.App.Tools().Storage().StorageRoot() + "/entities/users/" + state.Info().UserId() + "/" + input.EntityId
			cmd := exec.Command("convert", entityPath+".original", "-quality", imageQuality(len(data)), "-thumbnail", imageThumbSize(input.EntityId, data)+">", entityPath+".jpg")
			output, err := cmd.Output()
			if err != nil {
				log.Fatalf("Command execution failed: %v", err)
			}
			fmt.Printf("Command output:\n%s", output)
		} else {
			if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/users/"+state.Info().UserId(), data, input.EntityId, true); err != nil {
				log.Println(err)
				return nil, err
			}
		}
	} else {
		vm := models.Vm{MachineId: input.MachineId}.Pull(trx)
		app := models.App{Id: vm.AppId}.Pull(trx)
		if app.OwnerId != state.Info().UserId() {
			return nil, errors.New("you are not owner of this machine")
		}
		if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/users/"+vm.MachineId, data, input.EntityId+".original", true); err != nil {
			log.Println(err)
			return nil, err
		}
		if mimeType := http.DetectContentType(data); strings.HasPrefix(mimeType, "image/") {
			entityPath := a.App.Tools().Storage().StorageRoot() + "/entities/users/" + vm.MachineId + "/" + input.EntityId
			cmd := exec.Command("convert", entityPath+".original", "-quality", imageQuality(len(data)), "-thumbnail", imageThumbSize(input.EntityId, data)+">", entityPath+".jpg")
			output, err := cmd.Output()
			if err != nil {
				log.Fatalf("Command execution failed: %v", err)
			}
			fmt.Printf("Command output:\n%s", output)
		} else {
			if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/users/"+vm.MachineId, data, input.EntityId, true); err != nil {
				log.Println(err)
				return nil, err
			}
		}
	}
	return map[string]any{}, nil
}

// DeleteUserEntity /storage/deleteUserEntity check [ true false false ] access [ true false false false POST ]
func (a *Actions) DeleteUserEntity(state state.IState, input inputs_storage.DeleteUserEntityInput) (any, error) {
	trx := state.Trx()
	if input.MachineId == "" {
		if err := a.App.Tools().File().DeleteFileFromGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/users/"+state.Info().UserId(), input.EntityId, true); err != nil {
			log.Println(err)
			return nil, err
		}
	} else {
		vm := models.Vm{MachineId: input.MachineId}.Pull(trx)
		app := models.App{Id: vm.AppId}.Pull(trx)
		if app.OwnerId != state.Info().UserId() {
			return nil, errors.New("you are not owner of this machine")
		}
		if err := a.App.Tools().File().DeleteFileFromGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/users/"+vm.MachineId, input.EntityId, true); err != nil {
			log.Println(err)
			return nil, err
		}
	}
	return map[string]any{}, nil
}

// UploadPointEntity /storage/uploadPointEntity check [ true true true ] access [ true false false false POST ]
func (a *Actions) UploadPointEntity(state state.IState, input inputs_storage.UploadPointEntityInput) (any, error) {
	if state.Trx().GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) != "true" {
		if meta, err := state.Trx().GetJson("PointAccess::"+state.Info().PointId()+"::"+state.Info().UserId(), "metadata"); err != nil && !meta["uploadEntity"].(bool) {
			return nil, errors.New("access not permitted")
		}
	}
	data, err := base64.StdEncoding.DecodeString(input.Data)
	if err != nil {
		log.Println(err)
		return nil, err
	}
	if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/points/"+input.PointId, data, input.EntityId+".original", true); err != nil {
		log.Println(err)
		return nil, err
	}
	if mimeType := http.DetectContentType(data); strings.HasPrefix(mimeType, "image/") {
		entityPath := a.App.Tools().Storage().StorageRoot() + "/entities/points/" + input.PointId + "/" + input.EntityId
		cmd := exec.Command("convert", entityPath+".original", "-quality", imageQuality(len(data)), "-thumbnail", imageThumbSize(input.EntityId, data)+">", entityPath+".jpg")
		output, err := cmd.Output()
		if err != nil {
			log.Fatalf("Command execution failed: %v", err)
		}
		fmt.Printf("Command output:\n%s", output)
	} else {
		if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/points/"+input.PointId, data, input.EntityId, true); err != nil {
			log.Println(err)
			return nil, err
		}
	}
	future.Async(func() {
		a.App.Tools().Signaler().SignalGroup("storage/updatePointEntity", state.Info().PointId(), map[string]any{"pointId": state.Info().PointId(), "entityId": input.EntityId}, true, []string{})
	}, false)
	return map[string]any{}, nil
}

// UploadAppEntity /storage/uploadAppEntity check [ true true true ] access [ true false false false POST ]
func (a *Actions) UploadAppEntity(state state.IState, input inputs_storage.UploadAppEntityInput) (any, error) {
	app := models.App{Id: input.AppId}.Pull(state.Trx())
	if app.OwnerId != state.Info().UserId() {
		return nil, errors.New("you are not owner of app")
	}
	data, err := base64.StdEncoding.DecodeString(input.Data)
	if err != nil {
		log.Println(err)
		return nil, err
	}
	if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/apps/"+input.AppId, data, input.EntityId+".original", true); err != nil {
		log.Println(err)
		return nil, err
	}
	if mimeType := http.DetectContentType(data); strings.HasPrefix(mimeType, "image/") {
		entityPath := a.App.Tools().Storage().StorageRoot() + "/entities/apps/" + input.AppId + "/" + input.EntityId
		cmd := exec.Command("convert", entityPath+".original", "-quality", imageQuality(len(data)), "-thumbnail", imageThumbSize(input.EntityId, data)+">", entityPath+".jpg")
		output, err := cmd.Output()
		if err != nil {
			log.Fatalf("Command execution failed: %v", err)
		}
		fmt.Printf("Command output:\n%s", output)
	} else {
		if err := a.App.Tools().File().SaveDataToGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/apps/"+input.AppId, data, input.EntityId, true); err != nil {
			log.Println(err)
			return nil, err
		}
	}
	return map[string]any{}, nil
}

// DeletePointEntity /storage/deletePointEntity check [ true true true ] access [ true false false false POST ]
func (a *Actions) DeletePointEntity(state state.IState, input inputs_storage.DeletePointEntityInput) (any, error) {
	trx := state.Trx()
	if trx.GetLink("admin::"+state.Info().PointId()+"::"+state.Info().UserId()) == "" {
		return nil, errors.New("you are not admin")
	}
	if err := a.App.Tools().File().DeleteFileFromGlobalStorage(a.App.Tools().Storage().StorageRoot()+"/entities/points/"+state.Info().PointId(), input.EntityId, true); err != nil {
		log.Println(err)
		return nil, err
	}
	future.Async(func() {
		a.App.Tools().Signaler().SignalGroup("storage/updatePointEntity", state.Info().PointId(), map[string]any{"pointId": state.Info().PointId(), "entityId": input.EntityId}, true, []string{})
	}, false)
	return map[string]any{}, nil
}

// Download /storage/download check [ true true true ] access [ true false false false POST ]
func (a *Actions) Download(state state.IState, input inputs_storage.DownloadInput) (any, error) {
	trx := state.Trx()
	if !trx.HasObj("File", input.FileId) {
		return nil, errors.New("file not found")
	}
	var file = models.File{Id: input.FileId}.Pull(trx)
	if file.PointId != state.Info().PointId() {
		return nil, errors.New("access to file denied")
	}
	data, err := a.App.Tools().File().ReadFileFromStorage(a.App.Tools().Storage().StorageRoot(), state.Info().PointId(), file.Id)
	if err != nil {
		log.Println(err)
		return nil, err
	}
	return map[string]any{"data": data}, nil
}
