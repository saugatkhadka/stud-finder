namespace :api do
  namespace :v2 do
    resources :widgets, only: [:index, :show]
  end
end
