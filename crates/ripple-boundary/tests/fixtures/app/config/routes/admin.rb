namespace :admin do
  resources :reports, only: [:index]
end
