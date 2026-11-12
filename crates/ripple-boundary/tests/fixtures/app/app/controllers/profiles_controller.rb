class ProfilesController < ApplicationController
  # render inertia: "Commented/Out"
  def show
    render inertia: "Users/Show", props: { user: current_user }
  end
end
