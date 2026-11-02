package main

import (
	"context"
	"database/sql"
	"errors"
	"net/http"
	"sync"
)

var (
	userByToken  sync.Map // access_token -> *User
	ownerByToken sync.Map // access_token -> *Owner
	chairByToken sync.Map // access_token -> *Chair
)

func clearAuthCaches() {
	userByToken.Range(func(key, _ any) bool {
		userByToken.Delete(key)
		return true
	})
	ownerByToken.Range(func(key, _ any) bool {
		ownerByToken.Delete(key)
		return true
	})
	chairByToken.Range(func(key, _ any) bool {
		chairByToken.Delete(key)
		return true
	})
}

func appAuthMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		ctx := r.Context()
		c, err := r.Cookie("app_session")
		if errors.Is(err, http.ErrNoCookie) || c.Value == "" {
			writeError(w, http.StatusUnauthorized, errors.New("app_session cookie is required"))
			return
		}
		accessToken := c.Value
		if cached, ok := userByToken.Load(accessToken); ok {
			ctx = context.WithValue(ctx, "user", cached.(*User))
			next.ServeHTTP(w, r.WithContext(ctx))
			return
		}

		user := &User{}
		err = db.GetContext(ctx, user, "SELECT id FROM users WHERE access_token = ?", accessToken)
		if err != nil {
			if errors.Is(err, sql.ErrNoRows) {
				writeError(w, http.StatusUnauthorized, errors.New("invalid access token"))
				return
			}
			writeError(w, http.StatusInternalServerError, err)
			return
		}

		userByToken.Store(accessToken, user)
		ctx = context.WithValue(ctx, "user", user)
		next.ServeHTTP(w, r.WithContext(ctx))
	})
}

func ownerAuthMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		ctx := r.Context()
		c, err := r.Cookie("owner_session")
		if errors.Is(err, http.ErrNoCookie) || c.Value == "" {
			writeError(w, http.StatusUnauthorized, errors.New("owner_session cookie is required"))
			return
		}
		accessToken := c.Value
		if cached, ok := ownerByToken.Load(accessToken); ok {
			ctx = context.WithValue(ctx, "owner", cached.(*Owner))
			next.ServeHTTP(w, r.WithContext(ctx))
			return
		}

		owner := &Owner{}
		if err := db.GetContext(ctx, owner, "SELECT id FROM owners WHERE access_token = ?", accessToken); err != nil {
			if errors.Is(err, sql.ErrNoRows) {
				writeError(w, http.StatusUnauthorized, errors.New("invalid access token"))
				return
			}
			writeError(w, http.StatusInternalServerError, err)
			return
		}

		ownerByToken.Store(accessToken, owner)
		ctx = context.WithValue(ctx, "owner", owner)
		next.ServeHTTP(w, r.WithContext(ctx))
	})
}

func chairAuthMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		ctx := r.Context()
		c, err := r.Cookie("chair_session")
		if errors.Is(err, http.ErrNoCookie) || c.Value == "" {
			writeError(w, http.StatusUnauthorized, errors.New("chair_session cookie is required"))
			return
		}
		accessToken := c.Value
		if cached, ok := chairByToken.Load(accessToken); ok {
			ctx = context.WithValue(ctx, "chair", cached.(*Chair))
			next.ServeHTTP(w, r.WithContext(ctx))
			return
		}

		chair := &Chair{}
		err = db.GetContext(ctx, chair, "SELECT id FROM chairs WHERE access_token = ?", accessToken)
		if err != nil {
			if errors.Is(err, sql.ErrNoRows) {
				writeError(w, http.StatusUnauthorized, errors.New("invalid access token"))
				return
			}
			writeError(w, http.StatusInternalServerError, err)
			return
		}

		chairByToken.Store(accessToken, chair)
		ctx = context.WithValue(ctx, "chair", chair)
		next.ServeHTTP(w, r.WithContext(ctx))
	})
}
