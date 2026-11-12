Rails.application.routes.draw do
  root "home#index"

  devise_for :users
  mount Sidekiq::Web => "/sidekiq", as: :sidekiq

  concern :commentable do
    resources :comments, only: [:index, :create]
  end

  resources :photos, except: [:new, :edit, :update, :destroy], concerns: :commentable do
    member do
      post :like
    end
    collection do
      get :search
    end
    get :preview, on: :member
    resources :tags, only: :index, shallow: true
  end

  resource :profile, only: [:show, :update] do
    get :avatar
  end

  resources :articles, path: "posts", as: :posts, controller: "blog/articles", param: :slug, only: [:show]

  resources :boards, shallow: true, only: [:show] do
    resources :cards, only: [:index, :show]
  end

  namespace :admin do
    resources :users, only: :index
    get "stats" => "dashboard#stats", as: :stats
  end

  scope "/:locale", module: "localized", as: "loc" do
    get "help", to: "pages#help"
  end

  controller :sessions do
    get "login" => :new
    delete "logout", action: :destroy
  end

  get "about", to: "pages#about"
  get "legal/terms"
  post "/feedback", controller: "feedback", action: "create"
  match "status", to: "status#show", via: [:get, :head]
  get "old", to: redirect("/new")
  get "files/*path", to: "files#show", format: false

  constraints subdomain: "api" do
    defaults format: :json do
      get "ping", to: "health#ping"
    end
  end

  with_options controller: "reports" do |r|
    r.get "reports/daily", action: :daily
  end

  %w[faq contact].each do |page|
    get page, to: "pages##{page}"
  end

  shallow do
    resources :teams, only: [:new, :edit] do
      resources :members, only: [:new, :destroy]
    end
  end

  resources :events, only: [] do
    concerns :commentable
    get :upcoming, on: :collection
  end

  scope path: "/v", as: "versioned" do
    resources :notes, :links, only: :index
  end

  namespace :ops, path: "operations" do
    match "health", to: "checks#health", via: :all
  end

  if false
    get "never", to: "never#never"
  end

  draw :admin
  draw "api/v2"

  weird_macro :x
end
